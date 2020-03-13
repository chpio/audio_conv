use anyhow::{Context, Result};
use futures::prelude::*;
use glib::translate::ToGlibPtr;
use gstreamer::Element;
use gstreamer_audio::{prelude::*, AudioEncoder};
use gstreamer_base::prelude::*;
use std::path::{Path, PathBuf};

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
    let input = std::env::args().nth(1).expect("missing input");
    let output = std::env::args().nth(2).expect("missing output");
    futures::executor::block_on(
        futures::stream::iter(get_path_pairs(input.into(), output.into())).for_each_concurrent(
            num_cpus::get(),
            |(src, dest)| async move {
                if let Err(err) = transcode(src.as_path(), dest.as_path()).await {
                    println!("err {} => {}:\n{:?}", src.display(), dest.display(), err);
                }
            },
        ),
    );
    Ok(())
}

async fn transcode(src: &Path, dest: &Path) -> Result<()> {
    let file_src: gstreamer_base::BaseSrc = gmake("filesrc")?;
    let src_cstring = ToGlibPtr::<*const libc::c_char>::to_glib_none(src).1;
    let src_gstring = glib::GString::ForeignOwned(Some(src_cstring));
    file_src.set_property("location", &src_gstring)?;

    // encode into a tmp file first, then rename to actuall file name, that way we're writing
    // "whole" files to the intended file path, ignoring partial files in the mtime check
    let tmp_dest = dest.with_extension("tmp");
    let file_dest: gstreamer_base::BaseSink = gmake("filesink")?;
    let tmp_dest_cstring = ToGlibPtr::<*const libc::c_char>::to_glib_none(&tmp_dest).1;
    let tmp_dest_gstring = glib::GString::ForeignOwned(Some(tmp_dest_cstring));
    file_dest.set_property("location", &tmp_dest_gstring)?;
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
        // `audioconvert` converts the bitdepth
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

    pipeline
        .set_state(gstreamer::State::Playing)
        .context("Unable to set the pipeline to the `Playing` state")?;

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

    std::fs::rename(tmp_dest, dest)?;

    Ok(())
}
