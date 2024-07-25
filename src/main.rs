mod config;
mod tag;
mod ui;

use crate::config::{Config, Transcode};
use anyhow::{Context, Error, Result};
use futures::{pin_mut, prelude::*};
use glib::Boxed;
use gstreamer::{element_error, prelude::*, Element};
use gstreamer_base::prelude::*;
use std::{
	borrow::Cow,
	error::Error as StdError,
	fmt,
	fmt::Write as FmtWrite,
	path::{Path, PathBuf},
	result::Result as StdResult,
	sync::Arc,
	time::Duration,
};
use tokio::{fs, io::AsyncWriteExt, task, time::interval};

#[derive(Clone, Debug, Boxed)]
#[boxed_type(name = "GBoxErrorWrapper")]
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

fn gmake<T: IsA<Element>>(factory_name: &str, properties: &[(&str, &dyn ToValue)]) -> Result<T> {
	let builder = gstreamer::ElementFactory::make(factory_name);
	let builder = properties
		.into_iter()
		.fold(builder, |builder, (name, value)| {
			builder.property(name, value.to_value())
		});
	let res = builder
		.build()
		.with_context(|| format!("Could not make gstreamer Element \"{}\"", factory_name))?
		.downcast()
		.ok()
		.with_context(|| {
			format!(
				"Could not cast gstreamer Element \"{}\" into `{}`",
				factory_name,
				std::any::type_name::<T>()
			)
		})?;
	Ok(res)
}

#[derive(Debug, Clone)]
pub struct ConversionArgs {
	rel_from_path: PathBuf,
	transcode: Transcode,
}

fn get_conversion_args(config: &Config) -> impl Iterator<Item = Result<ConversionArgs>> + '_ {
	walkdir::WalkDir::new(&config.from)
		.into_iter()
		.filter_map(|e| e.ok())
		.filter(|e| e.file_type().is_file())
		.map(move |e| -> Result<Option<ConversionArgs>> {
			let from_bytes = path_to_bytes(e.path());

			let transcode = config
				.matches
				.iter()
				.filter(|m| {
					m.regexes
						.iter()
						.any(|regex| regex.is_match(from_bytes.as_ref()))
				})
				.map(|m| m.to.clone())
				.next();
			let transcode = if let Some(transcode) = transcode {
				transcode
			} else {
				return Ok(None);
			};

			let rel_path = e.path().strip_prefix(&config.from).with_context(|| {
				format!(
					"Unable to get relative path for {} from {}",
					e.path().display(),
					config.from.display()
				)
			})?;

			let mut to = config.to.join(&rel_path);
			to.set_extension(transcode.extension());

			let is_newer = {
				let from_mtime = e
					.metadata()
					.map_err(Error::new)
					.and_then(|md| md.modified().map_err(Error::new))
					.with_context(|| {
						format!(
							"Unable to get mtime for \"from\" file {}",
							e.path().display()
						)
					})?;
				let to_mtime = to.metadata().and_then(|md| md.modified());
				match to_mtime {
					Ok(to_mtime) => to_mtime < from_mtime,
					Err(err) if err.kind() == std::io::ErrorKind::NotFound => true,
					Err(err) => {
						return Err(err).with_context(|| {
							format!("Unable to get mtime for \"to\" file {}", to.display())
						})
					}
				}
			};

			if is_newer {
				Ok(Some(ConversionArgs {
					rel_from_path: rel_path.to_path_buf(),
					transcode,
				}))
			} else {
				Ok(None)
			}
		})
		.filter_map(|e| e.transpose())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
	task::LocalSet::new()
		.run_until(async move {
			let (ui_queue, ui_fut) = ui::init();

			let main_handle = async move {
				let ok = task::spawn_local(main_loop(ui_queue))
					.await
					.context("Main task failed")??;
				Result::<_>::Ok(ok)
			};

			let ui_handle = async move {
				let ok = task::spawn_local(ui_fut)
					.await
					.context("Ui task failed")?
					.context("Ui failed")?;
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
		gstreamer::tags::register::<tag::MbArtistId>();
		gstreamer::tags::register::<tag::MbAlbumArtistId>();

		let config = config::config().context("Could not get the config")?;

		let conv_args = get_conversion_args(&config)
			.collect::<Result<Vec<_>>>()
			.context("Failed loading dir structure")?;

		Ok((config, conv_args))
	})
	.await
	.context("Init task failed")??;

	let log_path = Path::new(".")
		.canonicalize()
		.context("Unable to canonicalize path to log file")?
		.join("audio-conv.log");

	ui_queue.push(ui::Msg::Init {
		task_len: conv_args.len(),
		log_path: log_path.clone(),
	});

	let concurrent_jobs = config.jobs.unwrap_or_else(|| num_cpus::get());

	stream::iter(conv_args.into_iter().enumerate())
		.map(Ok)
		.try_for_each_concurrent(concurrent_jobs, |(i, args)| {
			let config = &config;
			let ui_queue = &ui_queue;
			let log_path = &log_path;

			async move {
				ui_queue.push(ui::Msg::TaskStart {
					id: i,
					args: args.clone(),
				});

				match transcode(config, &args, i, ui_queue).await {
					Ok(()) => ui_queue.push(ui::Msg::TaskEnd { id: i }),
					Err(err) => {
						let err = err.context(format!(
							"Transcoding failed for {}",
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
						if let Err(write_err) = write!(&mut err_str, "{:?}\n", err) {
							let err = err.context(format!(
								"Unable to format transcoding error for logging (write error: {})",
								write_err
							));
							return Err(err);
						}

						log_file
							.write_all(err_str.as_ref())
							.await
							.map_err(|fs_err| {
								err.context(format!(
									"Unable to write transcoding error to log file (fs error: {})",
									fs_err
								))
							})?;

						ui_queue.push(ui::Msg::TaskError { id: i });
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
	args: &ConversionArgs,
	task_id: usize,
	queue: &ui::MsgQueue,
) -> Result<()> {
	let from_path = config.from.join(&args.rel_from_path);
	let mut to_path = config.to.join(&args.rel_from_path);

	fs::create_dir_all(
		to_path
			.parent()
			.with_context(|| format!("Could not get parent dir for {}", to_path.display()))?,
	)
	.await?;

	// encode into a tmp file first, then rename to actuall file name, that way we're writing
	// "whole" files to the intended file path, ignoring partial files in the mtime check
	let to_path_tmp = to_path.with_extension("tmp");

	rm_file_on_err(&to_path_tmp, async {
		match args.transcode {
			Transcode::Copy => {
				fs::copy(&from_path, &to_path_tmp).await.with_context(|| {
					format!(
						"Could not copy file from {} to {}",
						from_path.display(),
						to_path_tmp.display()
					)
				})?;
			}
			_ => {
				to_path.set_extension(args.transcode.extension());

				transcode_gstreamer(
					&from_path,
					&to_path_tmp,
					args.transcode.clone(),
					task_id,
					queue,
				)
				.await?
			}
		}

		fs::rename(&to_path_tmp, &to_path).await.with_context(|| {
			format!(
				"Could not rename temporary file {} to {}",
				to_path_tmp.display(),
				to_path.display()
			)
		})
	})
	.await
}

async fn transcode_gstreamer(
	from_path: &Path,
	to_path: &Path,
	transcode: Transcode,
	task_id: usize,
	queue: &ui::MsgQueue,
) -> Result<()> {
	let file_src: Element = gmake("filesrc", &[("location", &from_path)])?;

	let decodebin: Element = gmake("decodebin", &[])?;

	let src_elems: &[&Element] = &[&file_src, &decodebin];

	let pipeline = gstreamer::Pipeline::new();

	pipeline.add_many(src_elems)?;
	Element::link_many(src_elems)?;

	// downgrade pipeline RC to a weak RC to break the reference cycle
	let pipeline_weak = pipeline.downgrade();

	let to_path_clone = to_path.to_owned();
	decodebin.connect_pad_added(move |decodebin, src_pad| {
		let insert_sink = || -> Result<()> {
			let pipeline = match pipeline_weak.upgrade() {
				Some(pipeline) => pipeline,
				None => {
					// pipeline already destroyed... ignoring
					return Ok(());
				}
			};

			let is_audio = src_pad.current_caps().and_then(|caps| {
				caps.structure(0).map(|s| {
					let name = s.name();
					name.starts_with("audio/")
				})
			});
			match is_audio {
				None => {
					return Err(Error::msg(format!(
						"Failed to get media type from pad {}",
						src_pad.name()
					)));
				}
				Some(false) => {
					// not audio pad... ignoring
					return Ok(());
				}
				Some(true) => {}
			}

			let resample: Element = gmake(
				"audioresample",
				&[
					// quality from 0 to 10
					("quality", &10i32),
				],
			)?;

			let mut dest_elems = vec![
				resample,
				// `audioconvert` converts audio format, bitdepth, ...
				gmake("audioconvert", &[])?,
			];

			match &transcode {
				Transcode::Opus {
					bitrate,
					bitrate_type,
				} => {
					let encoder: Element = gmake(
						"opusenc",
						&[
							(
								"bitrate",
								&i32::from(*bitrate)
									.checked_mul(1_000)
									.context("Bitrate overflowed")?,
							),
							(
								"bitrate-type",
								match bitrate_type {
									config::BitrateType::Vbr => &"1",
									config::BitrateType::Cbr => &"0",
								},
							),
						],
					)?;

					dest_elems.push(encoder);
					dest_elems.push(gmake("oggmux", &[])?);
				}

				Transcode::Flac { compression } => {
					let encoder: Element =
						gmake("flacenc", &[("quality", &compression.to_string())])?;
					dest_elems.push(encoder);
				}

				Transcode::Mp3 {
					bitrate,
					bitrate_type,
				} => {
					let encoder: Element = gmake(
						"lamemp3enc",
						&[
							// target: "1" = "bitrate"
							("target", &"1"),
							("bitrate", &i32::from(*bitrate)),
							(
								"cbr",
								match bitrate_type {
									config::BitrateType::Vbr => &false,
									config::BitrateType::Cbr => &true,
								},
							),
						],
					)?;

					dest_elems.push(encoder);
					dest_elems.push(gmake("id3v2mux", &[])?);
				}

				Transcode::Copy => {
					// already handled outside this fn
					unreachable!();
				}
			};

			let file_dest: gstreamer_base::BaseSink =
				gmake("filesink", &[("location", &to_path_clone)])?;
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
				.static_pad("sink")
				.expect("1. dest element has no sinkpad");
			src_pad.link(&sink_pad)?;

			Ok(())
		};

		if let Err(err) = insert_sink() {
			let details = gstreamer::Structure::builder("error-details")
				.field("error", &GBoxErrorWrapper::new(err))
				.build();

			element_error!(
				decodebin,
				gstreamer::LibraryError::Failed,
				("Failed to insert sink"),
				details: details
			);
		}
	});

	let bus = pipeline.bus().context("Could not get bus for pipeline")?;

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
						let pipe_stop_res = pipeline.set_state(gstreamer::State::Null);

						let err: Error = err
							.details()
							.and_then(|details| {
								if details.name() != "error-details" {
									return None;
								}

								let err = details
									.get::<&GBoxErrorWrapper>("error")
									.unwrap()
									.clone()
									.into();
								Some(err)
							})
							.unwrap_or_else(|| {
								GErrorMessage {
									src: msg
										.src()
										.map(|s| String::from(s.path_string()))
										.unwrap_or_else(|| String::from("None")),
									error: err.error().to_string(),
									debug: err.debug().map(|gstring| gstring.into()),
									source: err.error(),
								}
								.into()
							});

						if let Err(pipe_err) = pipe_stop_res {
							let err = err.context(pipe_err).context(
								"Unable to set the pipeline to the `Null` state, after error",
							);
							Err(err)
						} else {
							Err(err)
						}
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
			.await?;

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
				.map(|time| time.nseconds());

			let ratio = dur.and_then(|dur| {
				if dur == 0 {
					return None;
				}

				let pos = decodebin
					.query_position::<ClockTime>()
					.map(|time| time.nseconds());

				pos.map(|pos| {
					let ratio = pos as f64 / dur as f64;
					ratio.clamp(0.0, 1.0)
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

	Ok(())
}

async fn rm_file_on_err<F, T>(path: &Path, f: F) -> Result<T>
where
	F: Future<Output = Result<T>>,
{
	match f.await {
		Err(err) => match fs::remove_file(path).await {
			Ok(()) => Err(err),
			Err(fs_err) if fs_err.kind() == std::io::ErrorKind::NotFound => Err(err),
			Err(fs_err) => {
				let err = err
					.context(fs_err)
					.context(format!("Removing file {} failed", path.display()));
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
