use futures::prelude::*;
use glib::error::{BoolError as GBoolError, Error as GError};
use glib::translate::ToGlibPtr;
use gstreamer::Element;
use gstreamer_audio::{prelude::*, AudioDecoder, AudioEncoder};
use gstreamer_base::prelude::*;
use std::borrow::Cow;
use std::path::{Path, PathBuf};

#[derive(Debug)]
enum Error {
    Str(Cow<'static, str>),
    GBoolError(GBoolError),
    GError(GError),
}

impl From<String> for Error {
    fn from(err: String) -> Error {
        Error::Str(err.into())
    }
}

impl From<&'static str> for Error {
    fn from(err: &'static str) -> Error {
        Error::Str(err.into())
    }
}

impl From<GBoolError> for Error {
    fn from(err: GBoolError) -> Error {
        Error::GBoolError(err)
    }
}

impl From<GError> for Error {
    fn from(err: GError) -> Error {
        Error::GError(err)
    }
}

fn gmake<T: IsA<Element>>(factory_name: &str) -> Result<T, Error> {
    let res = gstreamer::ElementFactory::make(factory_name, None)
        // TODO: passthrough err source
        .map_err(|_| format!("could not make \"{}\"", factory_name))?
        .downcast()
        .map_err(|_| {
            format!(
                "could not cast \"{}\" into `{}`",
                factory_name,
                std::any::type_name::<T>()
            )
        })?;
    Ok(res)
}

fn get_paths(input: PathBuf, output: PathBuf) -> impl Iterator<Item = (PathBuf, PathBuf)> {
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
        .inspect(|(_, out)| std::fs::create_dir_all(out.parent().unwrap()).unwrap())
}

fn main() -> Result<(), Error> {
    gstreamer::init().unwrap();
    let ctx = glib::MainContext::default();
    ctx.push_thread_default();
    let glib_loop = glib::MainLoop::new(Some(&ctx), false);

    let input = std::env::args().nth(1).expect("missing input");
    let output = std::env::args().nth(2).expect("missing output");

    let it = get_paths(input.into(), output.into());

    let f =
        futures::stream::iter(it).for_each_concurrent(num_cpus::get(), |(src, dest)| async move {
            if let Err(err) = transcode(src.as_path(), dest.as_path()).await {
                println!(
                    "err \"{}\" => \"{}\": {:?}",
                    src.to_string_lossy(),
                    dest.to_string_lossy(),
                    err
                );
            }
        });

    ctx.spawn_local(f);
    glib_loop.run();
    ctx.pop_thread_default();
    Ok(())
}

async fn transcode(src: &Path, dest: &Path) -> Result<(), Error> {
    let file_src: gstreamer_base::BaseSrc = gmake("filesrc")?;
    let src_cstring = ToGlibPtr::<*const libc::c_char>::to_glib_none(src).1;
    let src_gstring = glib::GString::ForeignOwned(Some(src_cstring));
    file_src.set_property("location", &src_gstring)?;

    let file_dest: gstreamer_base::BaseSink = gmake("filesink")?;
    let dest_cstring = ToGlibPtr::<*const libc::c_char>::to_glib_none(dest).1;
    let dest_gstring = glib::GString::ForeignOwned(Some(dest_cstring));
    file_dest.set_property("location", &dest_gstring)?;
    file_dest.set_sync(false);

    let parse: Element = gmake("flacparse")?;
    let dec: AudioDecoder = gmake("flacdec")?;
    let encoder: AudioEncoder = gmake("opusenc")?;
    encoder.set_property("bitrate", &160_000)?;
    // 0 = cbr; 1 = vbr
    encoder.set_property_from_str("bitrate-type", "1");

    let elems: &[&Element] = &[
        file_src.upcast_ref(),
        &parse,
        dec.upcast_ref(),
        &gmake("audioresample")?,
        encoder.upcast_ref(),
        &gmake("oggmux")?,
        file_dest.upcast_ref(),
    ];

    let pipeline = gstreamer::Pipeline::new(None);
    pipeline.add_many(elems)?;

    Element::link_many(elems)?;

    let bus = pipeline.get_bus().ok_or("pipe get bus")?;

    pipeline
        .set_state(gstreamer::State::Playing)
        .map_err(|_| "Unable to set the pipeline to the `Playing` state")?;

    gstreamer::BusStream::new(&bus)
        .map(|msg| {
            use gstreamer::MessageView;

            match msg.view() {
                MessageView::Eos(..) => Ok(false),
                MessageView::Error(err) => Err(err.get_error()),
                _ => Ok(true),
            }
        })
        .take_while(|e| {
            if let Ok(true) = e {
                futures::future::ready(true)
            } else {
                futures::future::ready(false)
            }
        })
        .try_for_each(|_| futures::future::ready(Ok(())))
        .await?;

    pipeline
        .set_state(gstreamer::State::Null)
        .map_err(|_| "Unable to set the pipeline to the `Null` state")?;

    Ok(())
}
