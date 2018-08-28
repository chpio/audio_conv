extern crate ffmpeg;

use std::env;
use std::path::Path;

use ffmpeg::{codec, filter, format, frame, media};
use ffmpeg::{rescale, Rescale};

fn filter(
    decoder: &codec::decoder::Audio,
    encoder: &codec::encoder::Audio,
) -> Result<filter::Graph, ffmpeg::Error> {
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

    println!("{}", filter.dump());

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

fn transcoder<P: AsRef<Path>>(
    ictx: &mut format::context::Input,
    octx: &mut format::context::Output,
    path: &P,
) -> Result<Transcoder, ffmpeg::Error> {
    let input = ictx
        .streams()
        .best(media::Type::Audio)
        .expect("could not find best audio stream");
    let mut decoder = input.codec().decoder().audio()?;
    let codec = ffmpeg::encoder::find(octx.format().codec(path, media::Type::Audio))
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

    let encoder = encoder.open_as(codec)?;
    output.set_parameters(&encoder);

    let filter = filter(&decoder, &encoder)?;

    Ok(Transcoder {
        stream: input.index(),
        filter: filter,
        decoder: decoder,
        encoder: encoder,
    })
}

fn main() -> Result<(), ffmpeg::Error> {
    ffmpeg::init()?;

    let input = env::args().nth(1).expect("missing input");
    let output = env::args().nth(2).expect("missing output");

    let mut ictx = format::input(&input)?;
    let mut octx = format::output(&output)?;
    let mut transcoder = transcoder(&mut ictx, &mut octx, &output)?;

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
    Ok(())
}
