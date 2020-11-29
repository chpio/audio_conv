mod config;

use crate::config::Config;
use anyhow::{Context, Error, Result};
use futures::{channel::mpsc, prelude::*};
use glib::{subclass::prelude::*, GBoxed, GString};
use gstreamer::{gst_element_error, prelude::*, Element};
use gstreamer_base::prelude::*;
use std::{
    borrow::Cow,
    error::Error as StdError,
    ffi, fmt,
    path::{Path, PathBuf},
    result::Result as StdResult,
    sync::Arc,
};

#[derive(Clone, Debug, GBoxed)]
#[gboxed(type_name = "GBoxErrorWrapper")]
struct GBoxErrorWrapper(Arc<Error>);

impl GBoxErrorWrapper {
    fn new(err: Error) -> Self {
        GBoxErrorWrapper(Arc::new(err))
    }
}

impl StdError for GBoxErrorWrapper {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        self.0.source()
    }
}

impl fmt::Display for GBoxErrorWrapper {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> StdResult<(), fmt::Error> {
        self.0.fmt(f)
    }
}

#[derive(Debug, derive_more::Display, derive_more::Error)]
#[display(fmt = "Received error from {}: {} (debug: {:?})", src, error, debug)]
struct GErrorMessage {
    src: String,
    error: String,
    debug: Option<String>,
    source: glib::Error,
}

fn gmake<T: IsA<Element>>(factory_name: &str) -> Result<T> {
    let res = gstreamer::ElementFactory::make(factory_name, None)
        .with_context(|| format!("could not make gstreamer Element \"{}\"", factory_name))?
        .downcast()
        .ok()
        .with_context(|| {
            format!(
                "could not cast gstreamer Element \"{}\" into `{}`",
                factory_name,
                std::any::type_name::<T>()
            )
        })?;
    Ok(res)
}

#[derive(Debug, Clone)]
struct ConvertionArgs {
    from: PathBuf,
    to: PathBuf,
    transcode: config::Transcode,
}

fn get_path_pairs(config: Config) -> impl Iterator<Item = ConvertionArgs> {
    walkdir::WalkDir::new(&config.from)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter_map(move |e| {
            let from_bytes = path_to_bytes(e.path());

            let transcode = config
                .matches
                .iter()
                .filter(|m| m.regex.is_match(from_bytes.as_ref()))
                .map(|m| m.to.clone())
                .next();
            let transcode = if let Some(transcode) = transcode {
                transcode
            } else {
                return None;
            };

            let mut to = config.to.join(e.path().strip_prefix(&config.from).unwrap());
            to.set_extension(transcode.extension());

            let is_newer = {
                // TODO: error handling
                let from_mtime = e.metadata().unwrap().modified().unwrap();
                let to_mtime = to.metadata().and_then(|md| md.modified());
                match to_mtime {
                    Ok(to_mtime) => to_mtime < from_mtime,
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => true,
                    Err(err) => panic!(err),
                }
            };

            if !is_newer {
                return None;
            }

            Some(ConvertionArgs {
                from: e.path().to_path_buf(),
                to,
                transcode,
            })
        })
}

fn main() -> Result<()> {
    gstreamer::init()?;
    let config = config::config().context("could not get the config")?;

    let (pair_tx, pair_rx) = mpsc::channel(16);

    // move blocking directory reading to an external thread
    let pair_producer = std::thread::spawn(|| {
        let produce_pairs = futures::stream::iter(get_path_pairs(config))
            .map(Ok)
            .forward(pair_tx)
            .map(|res| res.context("sending path pairs failed"));
        futures::executor::block_on(produce_pairs)
    });

    let transcoder = pair_rx.for_each_concurrent(num_cpus::get(), |args| async move {
        if let Err(err) = transcode(&args).await {
            println!(
                "err {} => {}:\n{:?}",
                args.from.display(),
                args.to.display(),
                err
            );
        }
    });
    futures::executor::block_on(transcoder);

    pair_producer
        .join()
        .expect("directory reading thread panicked")?;

    Ok(())
}

async fn transcode(args: &ConvertionArgs) -> Result<()> {
    let file_src: Element = gmake("filesrc")?;
    file_src.set_property("location", &path_to_gstring(&args.from))?;

    // encode into a tmp file first, then rename to actuall file name, that way we're writing
    // "whole" files to the intended file path, ignoring partial files in the mtime check
    let tmp_dest = args.to.with_extension("tmp");

    let decodebin: Element = gmake("decodebin")?;

    let src_elems: &[&Element] = &[&file_src, &decodebin];

    let pipeline = gstreamer::Pipeline::new(None);

    pipeline.add_many(src_elems)?;
    Element::link_many(src_elems)?;

    // downgrade pipeline RC to a weak RC to break the reference cycle
    let pipeline_weak = pipeline.downgrade();

    let transcode_args = args.transcode.clone();

    let tmp_dest_clone = tmp_dest.clone();

    decodebin.connect_pad_added(move |decodebin, src_pad| {
        let insert_sink = || -> Result<()> {
            let pipeline = match pipeline_weak.upgrade() {
                Some(pipeline) => pipeline,
                None => {
                    // pipeline already destroyed... ignoring
                    return Ok(());
                }
            };

            let is_audio = src_pad.get_current_caps().and_then(|caps| {
                caps.get_structure(0).map(|s| {
                    let name = s.get_name();
                    name.starts_with("audio/")
                })
            });
            match is_audio {
                None => {
                    return Err(Error::msg(format!(
                        "Failed to get media type from pad {}",
                        src_pad.get_name()
                    )));
                }
                Some(false) => {
                    // not audio pad... ignoring
                    return Ok(());
                }
                Some(true) => {}
            }

            let resample: Element = gmake("audioresample")?;
            // quality from 0 to 10
            resample.set_property("quality", &7)?;

            let mut dest_elems = vec![
                resample,
                // `audioconvert` converts audio format, bitdepth, ...
                gmake("audioconvert")?,
            ];

            match &transcode_args {
                config::Transcode::Opus {
                    bitrate,
                    bitrate_type,
                } => {
                    let encoder: Element = gmake("opusenc")?;
                    encoder.set_property(
                        "bitrate",
                        &i32::from(*bitrate)
                            .checked_mul(1_000)
                            .context("bitrate overflowed")?,
                    )?;
                    encoder.set_property_from_str(
                        "bitrate-type",
                        match bitrate_type {
                            config::OpusBitrateType::Vbr => "1",
                            config::OpusBitrateType::Cbr => "0",
                        },
                    );

                    dest_elems.push(encoder);
                    dest_elems.push(gmake("oggmux")?);
                }
            };

            let file_dest: gstreamer_base::BaseSink = gmake("filesink")?;
            file_dest.set_property("location", &path_to_gstring(&tmp_dest_clone))?;
            file_dest.set_sync(false);
            dest_elems.push(file_dest.upcast());

            let dest_elem_refs: Vec<_> = dest_elems.iter().collect();
            pipeline.add_many(&dest_elem_refs)?;
            Element::link_many(&dest_elem_refs)?;

            for e in &dest_elems {
                e.sync_state_with_parent()?;
            }

            let sink_pad = dest_elems
                .get(0)
                .unwrap()
                .get_static_pad("sink")
                .expect("1. dest element has no sinkpad");
            src_pad.link(&sink_pad)?;

            Ok(())
        };

        if let Err(err) = insert_sink() {
            let details = gstreamer::Structure::builder("error-details")
                .field("error", &GBoxErrorWrapper::new(err))
                .build();

            gst_element_error!(
                decodebin,
                gstreamer::LibraryError::Failed,
                ("Failed to insert sink"),
                details: details
            );
        }
    });

    let bus = pipeline.get_bus().context("pipe get bus")?;

    std::fs::create_dir_all(
        args.to
            .parent()
            .with_context(|| format!("could not get parent dir for {}", args.to.display()))?,
    )?;

    rm_file_on_err(&tmp_dest, async {
        pipeline
            .set_state(gstreamer::State::Playing)
            .context("Unable to set the pipeline to the `Playing` state")?;

        bus.stream()
            .map::<Result<bool>, _>(|msg| {
                use gstreamer::MessageView;

                match msg.view() {
                    MessageView::Eos(..) => {
                        // we need to actively stop pulling the stream, that's because stream will
                        // never end despite yielding an `Eos` message
                        Ok(false)
                    }
                    MessageView::Error(err) => {
                        pipeline.set_state(gstreamer::State::Null).context(
                            "Unable to set the pipeline to the `Null` state, after error",
                        )?;

                        let err = err
                            .get_details()
                            .and_then(|details| {
                                if details.get_name() != "error-details" {
                                    return None;
                                }

                                let err = details
                                    .get::<&GBoxErrorWrapper>("error")
                                    .unwrap()
                                    .map(|err| err.clone().into())
                                    .expect("error-details message without actual error");
                                Some(err)
                            })
                            .unwrap_or_else(|| {
                                GErrorMessage {
                                    src: msg
                                        .get_src()
                                        .map(|s| String::from(s.get_path_string()))
                                        .unwrap_or_else(|| String::from("None")),
                                    error: err.get_error().to_string(),
                                    debug: err.get_debug(),
                                    source: err.get_error(),
                                }
                                .into()
                            });
                        Err(err)
                    }
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
            .await
            .context("failed converting")?;

        pipeline
            .set_state(gstreamer::State::Null)
            .context("Unable to set the pipeline to the `Null` state")?;

        std::fs::rename(&tmp_dest, &args.to)?;

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

fn path_to_bytes(path: &Path) -> Cow<'_, [u8]> {
    // https://stackoverflow.com/a/59224987/5572146
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Cow::Borrowed(path.as_os_str().as_bytes())
    }

    #[cfg(windows)]
    {
        let mut buf = Vec::<u8>::new();
        // NOT TESTED
        // FIXME: test and post answer to https://stackoverflow.com/questions/38948669
        use std::os::windows::ffi::OsStrExt;
        buf.extend(
            path.as_os_str()
                .encode_wide()
                .map(|char| char.to_ne_bytes())
                .flatten(),
        );
        Cow::Owned(buf)
    }
}

fn path_to_gstring(path: &Path) -> GString {
    let buf = path_to_bytes(path);
    ffi::CString::new(buf)
        .expect("Path contained null byte")
        .into()
}
