use anyhow::{Result, anyhow};
use ctrlc;
use ffmpeg_next::{
    Dictionary, Error, Rational, codec::Context, device, encoder, ffi::EAGAIN, format, frame,
    packet, software::scaling,
};
use std::{sync::mpsc::channel, time};

fn main() -> Result<()> {
    // Find local devices that match
    let input = device::input::video()
        .find(|d| d.name() == "avfoundation")
        .ok_or(anyhow!("device not found"))?;

    // these settings are for the avfoundation device
    let mut opts = Dictionary::new();
    opts.set("pixel_format", "uyvy422");
    //opts.set("capture_raw_data", "1");
    //opts.set("video_size", "2560x1440");

    // Regardless of setting this, we always get 100k/1 fps
    opts.set("frame_rate", "30/1");

    // devices share same interface as codec, so we call format::open
    // the path is 'overloaded' to represent local device number
    let mut device = format::open_with("2", &input, opts).unwrap().input();

    // we're opening a 'format' which maps to an 'input' which wraps a context
    //
    // Timebase and rate are set for stream
    dbg!(device.stream(0).unwrap().time_base());
    dbg!(device.stream(0).unwrap().rate());
    let original_fps = Rational::new(30, 1);
    let original_timebase = Rational::new(1, 1_000_000);

    // Here we are creating the decoder and its context
    let mut dec_ctx = Context::from_parameters(device.stream(0).unwrap().parameters())?;

    // REF: https://lists.ffmpeg.org/pipermail/ffmpeg-devel/2023-March/307182.html
    // timebase is deprecated for decoder
    // but supposeduly framerate or pkt_timebase should be define, it is not here
    dbg!(dec_ctx.time_base());
    dbg!(dec_ctx.frame_rate());
    unsafe {
        dbg!((*dec_ctx.as_ptr()).pkt_timebase);
    }

    dbg!(original_fps);
    dec_ctx.set_time_base(original_fps.invert());
    let mut decoder = dec_ctx.decoder().video()?;

    // The opened decoder still have invalid frame and timebase
    dbg!(decoder.frame_rate());
    dbg!(decoder.time_base());

    //decoder.set_time_base(original_fps.invert());

    let codec =
        encoder::find_by_name("h264_videotoolbox").ok_or_else(|| anyhow!("Missing encoder"))?;
    //let codec = encoder::find(Id::H264).ok_or_else(|| anyhow!("Missing encoder"))?;
    let enc_ctx = Context::new_with_codec(codec);

    // The encoder here is a wrapper around encoding ctx
    // and provides methods more specifically for encoding
    // Video specialises further
    //
    // ctx -> encoder -> video -> encoder(video)
    // the last encoder(video) is what you get when you OPEN the context
    // slightly confusing...
    //

    let mut encoder = enc_ctx.encoder().video()?;

    let frame_rate = original_fps;
    // we can set params here OR via open_with
    encoder.set_width(decoder.width());
    encoder.set_height(decoder.height());
    encoder.set_aspect_ratio(decoder.aspect_ratio());
    encoder.set_frame_rate(Some(frame_rate));
    encoder.set_time_base(original_timebase);
    encoder.set_format(format::Pixel::YUV420P);

    let mut scaler = ffmpeg_next::software::converter(
        (decoder.width(), decoder.height()),
        format::Pixel::UYVY422,
        format::Pixel::YUV420P,
    )
    .unwrap();

    //let output = format::output(&output_file).unwrap();
    let mut output = format::output("/tmp/test.mp4").unwrap();
    let mut out_stream = output.add_stream(codec).unwrap();

    dbg!(out_stream.time_base());

    // Remember you have to open the encoding context!
    let mut opened = encoder.open().unwrap();
    out_stream.set_parameters(&opened);

    // Having set things up, we try and
    let mut frame_in = frame::Video::empty();
    let mut frame_out = frame::Video::empty();
    let mut packet = packet::Packet::empty();

    // write container headers before starting transcode
    output.write_header()?;
    let out_stream_tbase = output.stream(0).unwrap().time_base();

    let (tx, rx) = channel();
    ctrlc::set_handler(move || tx.send(()).expect("Could not send signal on channel."))
        .expect("Error setting Ctrl-C handler");

    let mut framecount = 0;

    loop {
        if rx.try_recv().is_ok() {
            break Ok(());
        }
        // iterate over packets from device
        let Some((_, p)) = device.packets().next() else {
            break Ok(());
        };

        // supposedly packet timebase is garbage!
        //
        //dbg!(&p.time_base());
        dbg!(&p.pts(), time::Instant::now());
        //dbg!(&p.duration());
        decoder.send_packet(&p)?;
        //dbg!(&p.time_base());
        //dbg!(&p.pts());

        match decoder.receive_frame(&mut frame_in) {
            Ok(_) => {}
            Err(Error::Other { errno }) if errno == EAGAIN => continue,
            Err(e) => break Err(e),
        };

        dbg!(frame_in.pts());
        dbg!(frame_in.packet().duration);

        // again this is currently garbage, docs state:
        // >> In the future, this field may be set on frames output by decoders or filters
        // >> but its value will be by default ignored on input to encoders or filters.
        // REF: https://www.ffmpeg.org/doxygen/7.0/structAVFrame.html#a36518d08c8e0ca31785e968add00fd07
        unsafe {
            dbg!((*frame_in.as_ptr()).time_base);
        }

        scaler.run(&frame_in, &mut frame_out)?;
        frame_out.set_pts(frame_in.pts());
        unsafe {
            //dbg!((*frame_in.as_mut_ptr()).time_base = original_fps.invert().into());
            //dbg!((*frame_in.as_ptr()).time_base);
        }

        // IS NONE
        //dbg!(frame_out.timestamp());
        // IS RIGHT ?
        //dbg!(frame_out.pts());

        match opened.send_frame(&frame_out) {
            Err(e) => break Err(e),
            Ok(_) => {
                // test
                match opened.receive_packet(&mut packet) {
                    Ok(_) => {}
                    Err(Error::Other { errno }) if errno == EAGAIN => continue,
                    Err(e) => break Err(e),
                };

                //dbg!(&packet.time_base());
                dbg!(&packet.pts());
                dbg!(original_fps.invert(), out_stream_tbase);
                packet.rescale_ts(original_timebase, out_stream_tbase);
                //dbg!(&packet.time_base());
                dbg!(&packet.pts());
                packet.write(&mut output);
            }
        }
    }?;

    output.write_trailer()?;
    Ok(())
}
