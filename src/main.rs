mod config;
mod ui;

use crate::config::Config;
use anyhow::{Context, Error, Result};
use futures::{future, pin_mut, prelude::*};
use glib::{subclass::prelude::*, GBoxed, GString};
use gstreamer::{gst_element_error, prelude::*, Element};
use gstreamer_base::prelude::*;
use std::{
    borrow::Cow,
    error::Error as StdError,
    ffi, fmt,
    fmt::Write as FmtWrite,
    path::{Path, PathBuf},
    result::Result as StdResult,
    sync::Arc,
    time::Duration,
};
use tokio::{fs, io::AsyncWriteExt, task, time::interval};

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
pub struct ConvertionArgs {
    rel_from_path: PathBuf,
    transcode: config::Transcode,
}

fn get_convertion_args(config: &Config) -> impl Iterator<Item = ConvertionArgs> + '_ {
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

            let rel_path = e.path().strip_prefix(&config.from).unwrap();

            let mut to = config.to.join(&rel_path);
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
                rel_from_path: rel_path.to_path_buf(),
                transcode,
            })
        })
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    task::LocalSet::new()
        .run_until(async move {
            let (ui_queue, ui_fut) = ui::init();

            let main_handle = async move {
                let ok = task::spawn_local(main_loop(ui_queue))
                    .await
                    .context("main task failed")??;
                Result::<_>::Ok(ok)
            };

            let ui_handle = async move {
                let ok = task::spawn_local(ui_fut)
                    .await
                    .context("ui task failed")?
                    .context("ui failed")?;
                Result::<_>::Ok(ok)
            };

            future::try_join(main_handle, ui_handle).await?;
            Ok(())
        })
        .await
}

async fn main_loop(ui_queue: ui::MsgQueue) -> Result<()> {
    let (config, conv_args) = task::spawn_blocking(|| -> Result<_> {
        gstreamer::init()?;
        let config = config::config().context("could not get the config")?;

        let conv_args = get_convertion_args(&config).collect::<Vec<_>>();

        Ok((config, conv_args))
    })
    .await
    .context("init task failed")??;

    let log_path = Path::new(".")
        .canonicalize()
        .context("unable to canonicalize path to log file")?
        .join("audio-conv.log");

    ui_queue.push(ui::Msg::Init {
        task_len: conv_args.len(),
        log_path: log_path.clone(),
    });

    stream::iter(conv_args.into_iter().enumerate())
        .map(Ok)
        .try_for_each_concurrent(num_cpus::get(), |(i, args)| {
            let config = config.clone();
            let msg_queue = ui_queue.clone();
            let log_path = &log_path;

            async move {
                msg_queue.push(ui::Msg::TaskStart {
                    id: i,
                    args: args.clone(),
                });

                match transcode(&config, &args, i, &msg_queue).await {
                    Ok(()) => msg_queue.push(ui::Msg::TaskEnd { id: i }),
                    Err(err) => {
                        let err = err.context(format!(
                            "failed transcoding \"{}\"",
                            args.rel_from_path.display()
                        ));

                        let mut log_file = match fs::OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(log_path)
                            .await
                        {
                            Ok(log_file) => log_file,
                            Err(fs_err) => {
                                let err = err.context(fs_err).context("Unable to open log file");
                                return Err(err);
                            }
                        };

                        let mut err_str = String::new();
                        write!(&mut err_str, "{:?}\n", err).context("TODO")?;

                        log_file
                            .write_all(err_str.as_ref())
                            .map_err(|fs_err| {
                                err.context(format!(
                                    "Unable to write transcoding error to log file (fs error: {})",
                                    fs_err
                                ))
                            })
                            .await?;

                        msg_queue.push(ui::Msg::TaskError { id: i });
                    }
                }

                Result::<_>::Ok(())
            }
        })
        .await?;

    ui_queue.push(ui::Msg::Exit);

    Ok(())
}

async fn transcode(
    config: &Config,
    args: &ConvertionArgs,
    task_id: usize,
    queue: &ui::MsgQueue,
) -> Result<()> {
    let from_path = config.from.join(&args.rel_from_path);
    let mut to_path = config.to.join(&args.rel_from_path);
    to_path.set_extension(args.transcode.extension());

    let file_src: Element = gmake("filesrc")?;
    file_src.set_property("location", &path_to_gstring(&from_path))?;

    // encode into a tmp file first, then rename to actuall file name, that way we're writing
    // "whole" files to the intended file path, ignoring partial files in the mtime check
    let to_path_tmp = to_path.with_extension("tmp");

    let decodebin: Element = gmake("decodebin")?;

    let src_elems: &[&Element] = &[&file_src, &decodebin];

    let pipeline = gstreamer::Pipeline::new(None);

    pipeline.add_many(src_elems)?;
    Element::link_many(src_elems)?;

    // downgrade pipeline RC to a weak RC to break the reference cycle
    let pipeline_weak = pipeline.downgrade();

    let transcode_args = args.transcode.clone();

    let to_path_tmp_clone = to_path_tmp.clone();

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
                            config::BitrateType::Vbr => "1",
                            config::BitrateType::Cbr => "0",
                        },
                    );

                    dest_elems.push(encoder);
                    dest_elems.push(gmake("oggmux")?);
                }
                config::Transcode::Mp3 {
                    bitrate,
                    bitrate_type,
                } => {
                    let encoder: Element = gmake("lamemp3enc")?;
                    // target: "1" = "bitrate"
                    encoder.set_property_from_str("target", "1");
                    encoder.set_property("bitrate", &i32::from(*bitrate))?;
                    encoder.set_property(
                        "cbr",
                        match bitrate_type {
                            config::BitrateType::Vbr => &false,
                            config::BitrateType::Cbr => &true,
                        },
                    )?;

                    dest_elems.push(encoder);
                    dest_elems.push(gmake("id3v2mux")?);
                }
            };

            let file_dest: gstreamer_base::BaseSink = gmake("filesink")?;
            file_dest.set_property("location", &path_to_gstring(&to_path_tmp_clone))?;
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

    fs::create_dir_all(
        to_path
            .parent()
            .with_context(|| format!("could not get parent dir for {}", to_path.display()))?,
    )
    .await?;

    rm_file_on_err(&to_path_tmp, async {
        pipeline
            .set_state(gstreamer::State::Playing)
            .context("Unable to set the pipeline to the `Playing` state")?;

        let stream_processor = async {
            bus.stream()
                .map::<Result<bool>, _>(|msg| {
                    use gstreamer::MessageView;

                    match msg.view() {
                        // MessageView::Progress() => {

                        // }
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

            Result::<_>::Ok(())
        };
        pin_mut!(stream_processor);

        let mut progress_interval = interval(Duration::from_millis(ui::UPDATE_INTERVAL_MILLIS / 2));
        let progress_processor = async {
            use gstreamer::ClockTime;

            loop {
                progress_interval.tick().await;

                let dur = decodebin
                    .query_duration::<ClockTime>()
                    .and_then(|time| time.nanoseconds());

                let ratio = dur.and_then(|dur| {
                    if dur == 0 {
                        return None;
                    }

                    let pos = decodebin
                        .query_position::<ClockTime>()
                        .and_then(|time| time.nanoseconds());

                    pos.map(|pos| {
                        let ratio = pos as f64 / dur as f64;
                        ratio.max(0.0).min(1.0)
                    })
                });

                if let Some(ratio) = ratio {
                    queue.push(ui::Msg::TaskProgress { id: task_id, ratio });
                }
            }

            #[allow(unreachable_code)]
            Result::<_>::Ok(())
        };
        pin_mut!(progress_processor);

        future::try_select(stream_processor, progress_processor)
            .await
            .map_err(|err| err.factor_first().0)?;

        pipeline
            .set_state(gstreamer::State::Null)
            .context("Unable to set the pipeline to the `Null` state")?;

        fs::rename(&to_path_tmp, &to_path).await?;

        Ok(())
    })
    .await
}

async fn rm_file_on_err<F, T>(path: &Path, f: F) -> F::Output
where
    F: Future<Output = Result<T>>,
{
    match f.await {
        Err(err) => match fs::remove_file(path).await {
            Ok(..) => Err(err),
            Err(fs_err) if fs_err.kind() == std::io::ErrorKind::NotFound => Err(err),
            Err(fs_err) => {
                let err = err
                    .context(fs_err)
                    .context(format!("removing {} failed", path.display()));
                Err(err)
            }
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
        // NOT TESTED
        // FIXME: test and post answer to https://stackoverflow.com/questions/38948669
        use std::os::windows::ffi::OsStrExt;
        let buf: Vec<u8> = path
            .as_os_str()
            .encode_wide()
            .map(|char| char.to_ne_bytes())
            .flatten()
            .collect();
        Cow::Owned(buf)
    }
}

fn path_to_gstring(path: &Path) -> GString {
    let buf = path_to_bytes(path);
    ffi::CString::new(buf)
        .expect("Path contained null byte")
        .into()
}
