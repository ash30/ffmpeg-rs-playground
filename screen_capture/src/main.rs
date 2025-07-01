use anyhow::{Result, anyhow};
use clap::Parser;
use ctrlc;
use ffmpeg_next::{
    Dictionary, Error, Rational, codec::Context, device, encoder, ffi::EAGAIN, format, frame,
    packet,
};
use std::path::PathBuf;
use std::sync::mpsc::{TryRecvError, channel};

// get list of devices via `ffmpeg -f avfoundation -list_devices true -i ""`

#[derive(Parser)]
struct Cli {
    #[arg(short, long)]
    device: String,

    #[arg(short, long, value_name = "FILE")]
    output_path: PathBuf,
}

fn main() -> Result<()> {
    let args = Cli::parse();

    let input = device::input::video()
        .find(|d| d.name() == "avfoundation")
        .ok_or(anyhow!("device not found"))?;

    let framerate = Rational::new(30, 1);

    let mut opts = Dictionary::new();
    opts.set("pixel_format", "uyvy422");
    opts.set("frame_rate", "30/1");

    // devices share same interface as codec, so we call format::open
    // the path is 'overloaded' to represent local device number
    let mut device = format::open_with(&args.device, &input, opts)
        .unwrap()
        .input();
    let in_stream_timebase = device.stream(0).unwrap().time_base();

    // Here we are creating the decoder and its context
    let mut dec_ctx = Context::from_parameters(device.stream(0).unwrap().parameters())?;

    dec_ctx.set_time_base(framerate.invert());
    let mut decoder = dec_ctx.decoder().video()?;

    let codec =
        encoder::find_by_name("h264_videotoolbox").ok_or_else(|| anyhow!("Missing encoder"))?;
    let enc_ctx = Context::new_with_codec(codec);

    let mut encoder = enc_ctx.encoder().video()?;

    let frame_rate = framerate;
    encoder.set_width(decoder.width());
    encoder.set_height(decoder.height());
    encoder.set_aspect_ratio(decoder.aspect_ratio());
    encoder.set_frame_rate(Some(frame_rate));
    encoder.set_time_base(in_stream_timebase);
    encoder.set_format(format::Pixel::YUV420P);

    let mut scaler = ffmpeg_next::software::converter(
        (decoder.width(), decoder.height()),
        format::Pixel::UYVY422,
        format::Pixel::YUV420P,
    )
    .unwrap();

    let mut output = format::output(&args.output_path).unwrap();
    let mut out_stream = output.add_stream(codec).unwrap();

    // Remember you have to open the encoding context!
    let mut opened = encoder.open().unwrap();
    out_stream.set_parameters(&opened);

    let mut frame_in = frame::Video::empty();
    let mut frame_out = frame::Video::empty();
    let mut packet = packet::Packet::empty();

    // write container headers before starting transcode
    output.write_header()?;
    let out_stream_timebase = output.stream(0).unwrap().time_base();

    let (tx, rx) = channel();
    ctrlc::set_handler(move || tx.send(()).expect("Could not send signal on channel."))
        .expect("Error setting Ctrl-C handler");

    // we reset pts to zero for live stream
    let mut packets_in = device.packets().peekable();
    let offset = packets_in.peek().and_then(|(_, p)| p.pts()).unwrap();

    while let Err(TryRecvError::Empty) = rx.try_recv() {
        let Some((_, p)) = packets_in.next() else {
            break;
        };

        decoder.send_packet(&p)?;
        match decoder.receive_frame(&mut frame_in) {
            Err(Error::Other { errno }) if errno == EAGAIN => continue,
            Err(e) => return Err(e.into()),
            _ => {}
        };

        // Resample due to format change
        scaler.run(&frame_in, &mut frame_out)?;
        frame_out.set_pts(frame_in.pts().map(|ts| ts - offset));

        match opened.send_frame(&frame_out) {
            Err(e) => return Err(e.into()),
            Ok(_) => {
                // test
                match opened.receive_packet(&mut packet) {
                    Ok(_) => {}
                    Err(Error::Other { errno }) if errno == EAGAIN => continue,
                    Err(e) => return Err(e.into()),
                };
                packet.rescale_ts(in_stream_timebase, out_stream_timebase);
                packet.write(&mut output)?;
            }
        }
    }

    output.write_trailer()?;
    Ok(())
}
