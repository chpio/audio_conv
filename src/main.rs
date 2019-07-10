use std::alloc;
use std::env;
use std::path::Path;
use std::{fs, io};

use ffmpeg_next::{self as ffmpeg, codec, filter, format, frame, media};
use rayon::prelude::*;

#[global_allocator]
static ALLOCATOR: alloc::System = alloc::System;

#[derive(Debug)]
enum Error {
    Io(io::Error),
    Ffmpeg(ffmpeg::Error),
    String(String),
    Str(&'static str),
}

impl From<ffmpeg::Error> for Error {
    fn from(v: ffmpeg::Error) -> Error {
        Error::Ffmpeg(v)
    }
}

impl From<io::Error> for Error {
    fn from(v: io::Error) -> Error {
        Error::Io(v)
    }
}

impl From<String> for Error {
    fn from(v: String) -> Error {
        Error::String(v)
    }
}

impl From<&'static str> for Error {
    fn from(v: &'static str) -> Error {
        Error::Str(v)
    }
}

fn filter(
    decoder: &codec::decoder::Audio,
    encoder: &codec::encoder::Audio,
) -> Result<filter::Graph, Error> {
    let mut filter = filter::Graph::new();

    let args = format!(
        "time_base={}:sample_rate={}:sample_fmt={}:channel_layout=0x{:x}",
        decoder.time_base(),
        decoder.rate(),
        decoder.format().name(),
        decoder.channel_layout().bits()
    );

    filter.add(&filter::find("abuffer").unwrap(), "in", &args)?;
    filter.add(&filter::find("abuffersink").unwrap(), "out", "")?;

    {
        let mut out = filter.get("out").unwrap();

        out.set_sample_format(encoder.format());
        out.set_channel_layout(encoder.channel_layout());
        out.set_sample_rate(encoder.rate());
    }

    filter.output("in", 0)?.input("out", 0)?.parse("anull")?;
    filter.validate()?;

    if let Some(codec) = encoder.codec() {
        if !codec
            .capabilities()
            .contains(ffmpeg::codec::capabilities::VARIABLE_FRAME_SIZE)
        {
            filter
                .get("out")
                .unwrap()
                .sink()
                .set_frame_size(encoder.frame_size());
        }
    }

    Ok(filter)
}

struct Transcoder {
    stream: usize,
    filter: filter::Graph,
    decoder: codec::decoder::Audio,
    encoder: codec::encoder::Audio,
}

fn transcoder(
    ictx: &mut format::context::Input,
    octx: &mut format::context::Output,
) -> Result<Transcoder, Error> {
    let input = ictx
        .streams()
        .best(media::Type::Audio)
        .expect("could not find best audio stream");
    let mut decoder = input.codec().decoder().audio()?;

    let codec = ffmpeg::encoder::find(octx.format().codec(&"", media::Type::Audio))
        .expect("failed to find encoder")
        .audio()?;
    let global = octx
        .format()
        .flags()
        .contains(ffmpeg::format::flag::GLOBAL_HEADER);

    decoder.set_parameters(input.parameters())?;

    let mut output = octx.add_stream(codec)?;
    let mut encoder = output.codec().encoder().audio()?;

    let channel_layout = codec
        .channel_layouts()
        .map(|cls| cls.best(decoder.channel_layout().channels()))
        .unwrap_or(ffmpeg::channel_layout::STEREO);

    if global {
        encoder.set_flags(ffmpeg::codec::flag::GLOBAL_HEADER);
    }

    encoder.set_rate(48_000); //decoder.rate() as i32);
    encoder.set_channel_layout(channel_layout);
    encoder.set_channels(channel_layout.channels());
    encoder.set_format(
        codec
            .formats()
            .expect("unknown supported formats")
            .next()
            .unwrap(),
    );

    encoder.set_time_base((1, 48_000));
    output.set_time_base((1, 48_000));

    let mut encode_dict = ffmpeg::Dictionary::new();
    encode_dict.set("vbr", "on");
    encoder.set_bit_rate(64_000);
    let encoder = encoder.open_as_with(codec, encode_dict)?;
    output.set_parameters(&encoder);

    let filter = filter(&decoder, &encoder)?;

    Ok(Transcoder {
        stream: input.index(),
        filter,
        decoder,
        encoder,
    })
}

fn transcode(input: &Path, output: &Path) -> Result<(), Error> {
    let mut ictx = format::input(&input)?;
    let original_extension = output
        .extension()
        .expect("file without extension")
        .to_string_lossy();
    let output_tmp = output.with_extension("tmp");
    let mut octx = format::output_as(&output_tmp, &original_extension)?;
    let mut transcoder = transcoder(&mut ictx, &mut octx)?;

    octx.set_metadata(ictx.metadata().to_owned());
    octx.write_header()?;

    let in_time_base = transcoder.decoder.time_base();

    let mut frame = frame::Audio::empty();
    let mut encoded = ffmpeg::Packet::empty();

    for (stream, mut packet) in ictx.packets() {
        if stream.index() != transcoder.stream {
            continue;
        }

        packet.rescale_ts(stream.time_base(), in_time_base);

        if let Ok(true) = transcoder.decoder.decode(&packet, &mut frame) {
            transcoder.filter.get("in").unwrap().source().add(&frame)?;

            while let Ok(..) = transcoder
                .filter
                .get("out")
                .unwrap()
                .sink()
                .frame(&mut frame)
            {
                if let Ok(true) = transcoder.encoder.encode(&frame, &mut encoded) {
                    encoded.set_stream(0);
                    encoded.write_interleaved(&mut octx)?;
                }
                unsafe {
                    ffmpeg::ffi::av_frame_unref(frame.as_mut_ptr());
                }
            }
        }
    }

    transcoder.filter.get("in").unwrap().source().flush()?;

    while let Ok(..) = transcoder
        .filter
        .get("out")
        .unwrap()
        .sink()
        .frame(&mut frame)
    {
        if let Ok(true) = transcoder.encoder.encode(&frame, &mut encoded) {
            encoded.set_stream(0);
            encoded.write_interleaved(&mut octx)?;
        }
    }

    if let Ok(true) = transcoder.encoder.flush(&mut encoded) {
        encoded.set_stream(0);
        encoded.write_interleaved(&mut octx)?;
    }

    octx.write_trailer()?;

    fs::rename(output_tmp, output)?;

    Ok(())
}

fn transcode_path(input: &Path, output: &Path) -> Result<(), Error> {
    input.read_dir()?.par_bridge().try_for_each(|entry| {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let new_input = input.join(entry.file_name());
        let mut new_output = output.join(entry.file_name());
        if file_type.is_dir() {
            transcode_path(new_input.as_ref(), new_output.as_ref())?;
        } else if file_type.is_file() {
            if entry.path().extension().unwrap() != "flac" {
                // println!("not flac input: {:?}", entry.path());
                return Ok(());
            }
            fs::create_dir_all(&output)?;
            new_output.set_extension("opus");
            let in_mtime = new_input.metadata()?.modified()?;
            let out_mtime = new_output.metadata().and_then(|md| md.modified());
            match out_mtime {
                Ok(out_mtime) => {
                    if out_mtime < in_mtime {
                        transcode(new_input.as_ref(), new_output.as_ref())?;
                    }
                }
                Err(e) => {
                    if e.kind() == io::ErrorKind::NotFound {
                        transcode(new_input.as_ref(), new_output.as_ref())?;
                    } else {
                        return Err(e.into());
                    }
                }
            }
        } else {
            Err(format!(
                "Unsupported file type `{:?}` (maybe symlink?)",
                new_input
            ))?;
        }
        Ok(())
    })
}

fn main() -> Result<(), Error> {
    ffmpeg::init()?;

    let input = env::args().nth(1).expect("missing input");
    let output = env::args().nth(2).expect("missing output");
    transcode_path(input.as_ref(), output.as_ref())?;

    Ok(())
}
