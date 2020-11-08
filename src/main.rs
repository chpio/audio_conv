mod config;

use anyhow::{Context, Result};
use futures::{channel::mpsc, prelude::*};
use glib::GString;
use gstreamer::Element;
use gstreamer_audio::{prelude::*, AudioEncoder};
use gstreamer_base::prelude::*;
use std::{
    ffi,
    path::{Path, PathBuf},
};

fn gmake<T: IsA<Element>>(factory_name: &str) -> Result<T> {
    let res = gstreamer::ElementFactory::make(factory_name, None)
        .with_context(|| format!("could not make {}", factory_name))?
        .downcast()
        .ok()
        .with_context(|| {
            format!(
                "could not cast {} into `{}`",
                factory_name,
                std::any::type_name::<T>()
            )
        })?;
    Ok(res)
}

fn get_path_pairs(input: PathBuf, output: PathBuf) -> impl Iterator<Item = (PathBuf, PathBuf)> {
    walkdir::WalkDir::new(input.as_path())
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter(|e| {
            e.path()
                .extension()
                .map(|ext| ext == "flac")
                .unwrap_or(false)
        })
        .map(move |e| {
            let mut out = output.join(e.path().strip_prefix(&input).unwrap());
            out.set_extension("opus");
            (e, out)
        })
        .filter(|(e, out)| {
            let in_mtime = e.metadata().unwrap().modified().unwrap();
            let out_mtime = out.metadata().and_then(|md| md.modified());
            match out_mtime {
                Ok(out_mtime) => out_mtime < in_mtime,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => true,
                Err(err) => panic!(err),
            }
        })
        .map(|(e, out)| (e.into_path(), out))
}

fn main() -> Result<()> {
    gstreamer::init()?;
    let config = config::config().context("could not get the config")?;

    let (pair_tx, pair_rx) = mpsc::channel(16);

    // move blocking directory reading to an external thread
    let pair_producer = std::thread::spawn(|| {
        let produce_pairs = futures::stream::iter(get_path_pairs(config.from, config.to))
            .map(Ok)
            .forward(pair_tx)
            .map(|res| res.context("sending path pairs failed"));
        futures::executor::block_on(produce_pairs)
    });

    let transcoder = pair_rx.for_each_concurrent(num_cpus::get(), |(src, dest)| async move {
        if let Err(err) = transcode(src.as_path(), dest.as_path()).await {
            println!("err {} => {}:\n{:?}", src.display(), dest.display(), err);
        }
    });
    futures::executor::block_on(transcoder);

    pair_producer
        .join()
        .expect("directory reading thread panicked")?;

    Ok(())
}

async fn transcode(src: &Path, dest: &Path) -> Result<()> {
    let file_src: gstreamer_base::BaseSrc = gmake("filesrc")?;
    file_src.set_property("location", &path_to_gstring(src))?;

    // encode into a tmp file first, then rename to actuall file name, that way we're writing
    // "whole" files to the intended file path, ignoring partial files in the mtime check
    let tmp_dest = dest.with_extension("tmp");
    let file_dest: gstreamer_base::BaseSink = gmake("filesink")?;
    file_dest.set_property("location", &path_to_gstring(&tmp_dest))?;
    file_dest.set_sync(false);

    let resample: Element = gmake("audioresample")?;
    // quality from 0 to 10
    resample.set_property("quality", &7)?;

    let encoder: AudioEncoder = gmake("opusenc")?;
    encoder.set_property("bitrate", &160_000)?;
    // 0 = cbr; 1 = vbr
    encoder.set_property_from_str("bitrate-type", "1");

    let elems: &[&Element] = &[
        file_src.upcast_ref(),
        &gmake("flacparse")?,
        &gmake("flacdec")?,
        &resample,
        // `audioconvert` converts audio format, bitdepth, ...
        &gmake("audioconvert")?,
        encoder.upcast_ref(),
        &gmake("oggmux")?,
        file_dest.upcast_ref(),
    ];

    let pipeline = gstreamer::Pipeline::new(None);
    pipeline.add_many(elems)?;

    Element::link_many(elems)?;

    let bus = pipeline.get_bus().context("pipe get bus")?;

    std::fs::create_dir_all(
        dest.parent()
            .with_context(|| format!("could not get parent dir for {}", dest.display()))?,
    )?;

    rm_file_on_err(&tmp_dest, async {
        pipeline
            .set_state(gstreamer::State::Playing)
            .context("Unable to set the pipeline to the `Playing` state")?;

        bus.stream()
            .map(|msg| {
                use gstreamer::MessageView;

                match msg.view() {
                    // we need to actively stop pulling the stream, that's because stream will
                    // never end despite yielding an `Eos` message
                    MessageView::Eos(..) => Ok(false),
                    MessageView::Error(err) => Err(err.get_error()),
                    _ => Ok(true),
                }
            })
            .take_while(|e| {
                if let Ok(false) = e {
                    futures::future::ready(false)
                } else {
                    futures::future::ready(true)
                }
            })
            .try_for_each(|_| futures::future::ready(Ok(())))
            .await?;

        pipeline
            .set_state(gstreamer::State::Null)
            .context("Unable to set the pipeline to the `Null` state")?;

        std::fs::rename(&tmp_dest, dest)?;

        Ok(())
    })
    .await
}

async fn rm_file_on_err<F, T>(path: &Path, f: F) -> F::Output
where
    F: Future<Output = Result<T>>,
{
    match f.await {
        Err(err) => match std::fs::remove_file(path) {
            Ok(..) => Err(err),
            Err(rm_err) if rm_err.kind() == std::io::ErrorKind::NotFound => Err(err),
            Err(rm_err) => Err(rm_err)
                .context(format!("removing {}", path.display()))
                .context(err),
        },
        res @ Ok(..) => res,
    }
}

fn path_to_gstring(path: &Path) -> GString {
    let buf = {
        let mut buf = Vec::<u8>::new();

        // https://stackoverflow.com/a/59224987/5572146
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            buf.extend(path.as_os_str().as_bytes());
        }

        #[cfg(windows)]
        {
            // NOT TESTED
            // FIXME: test and post answer to https://stackoverflow.com/questions/38948669
            use std::os::windows::ffi::OsStrExt;
            buf.extend(
                path.as_os_str()
                    .encode_wide()
                    .map(|char| char.to_ne_bytes())
                    .flatten(),
            );
        }

        buf
    };

    ffi::CString::new(buf).unwrap().into()
}
