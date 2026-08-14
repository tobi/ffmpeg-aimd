use ffmpeg::{codec, format, frame, media, Rational};
use ffmpeg_next as ffmpeg;
use memmap2::{Mmap, MmapOptions};
use motorsport_telemetry::open;
use std::error::Error;
use std::ffi::CString;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::ptr;
use std::time::{Duration, Instant};

const DEFAULT_INPUT_DIR: &str = "~/Documents/upload";
const DEFAULT_HEIGHT: u32 = 720;
const DEFAULT_BITRATE_KBPS: u32 = 3_000;

#[derive(Debug, Clone, Copy)]
struct TimeRange {
    from: f64,
    to: f64,
}

#[derive(Debug, Clone)]
struct Config {
    inputs: Vec<PathBuf>,
    output_dir: Option<PathBuf>,
    height: u32,
    bitrate_kbps: u32,
    fps: Option<Rational>,
    range: Option<TimeRange>,
    extract_telemetry: bool,
    extract_output: Option<PathBuf>,
    overwrite: bool,
    cpu_force: bool,
}

struct InputStream {
    index: usize,
    kind: media::Type,
    time_base: Rational,
    parameters: codec::Parameters,
    frames: i64,
}

#[derive(Debug)]
struct CopyStream {
    input_index: usize,
    output_index: usize,
    input_time_base: Rational,
    output_time_base: Rational,
}

#[derive(Debug, Clone, Copy)]
struct Atom {
    start: usize,
    payload: usize,
    end: usize,
    kind: [u8; 4],
}

#[derive(Debug)]
struct AimdTrack {
    atom: Atom,
    chunks: Vec<(usize, usize)>,
}

fn boxed(message: impl Into<String>) -> Box<dyn Error> {
    message.into().into()
}

fn main() -> Result<(), Box<dyn Error>> {
    let config = parse_args(std::env::args_os().skip(1))?;
    if config.extract_telemetry {
        return extract_telemetry_files(&config);
    }
    ffmpeg::init().map_err(|error| boxed(format!("initialising FFmpeg: {error}")))?;
    let inputs = discover_inputs(&config.inputs)?;
    if inputs.is_empty() {
        return Err(boxed("no .mp4 inputs found"));
    }

    let output_dir = config
        .output_dir
        .clone()
        .unwrap_or_else(|| inputs[0].parent().unwrap_or(Path::new(".")).join("reduced"));
    fs::create_dir_all(&output_dir)?;

    let mut failures = Vec::new();
    for input in inputs {
        match process_one(&input, &output_dir, &config) {
            Ok(output) => println!("{} -> {}", input.display(), output.display()),
            Err(error) => {
                eprintln!("{}: {error}", input.display());
                failures.push(input);
            }
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(boxed(format!("{} file(s) failed", failures.len())))
    }
}

fn parse_args<I>(args: I) -> Result<Config, Box<dyn Error>>
where
    I: IntoIterator<Item = std::ffi::OsString>,
{
    let mut positionals = Vec::new();
    let mut output_dir = None;
    let mut height = DEFAULT_HEIGHT;
    let mut bitrate_kbps = DEFAULT_BITRATE_KBPS;
    let mut fps = None;
    let mut extract_telemetry = false;
    let mut overwrite = false;
    let mut cpu_force = false;
    let mut args = args.into_iter();

    while let Some(arg) = args.next() {
        let text = arg.to_string_lossy();
        if text == "--help" || text == "-h" {
            print_usage();
            std::process::exit(0);
        } else if text == "--cpu" {
            cpu_force = true;
        } else if text == "--overwrite" {
            overwrite = true;
        } else if text == "--extract-telemetry" {
            extract_telemetry = true;
        } else if let Some(value) = text.strip_prefix("--res=") {
            height = parse_positive(value, "--res")?;
        } else if text == "--res" {
            height = parse_positive(&next_arg(&mut args, "--res")?, "--res")?;
        } else if let Some(value) = text.strip_prefix("--bitrate=") {
            bitrate_kbps = parse_positive(value, "--bitrate")?;
        } else if text == "--bitrate" {
            bitrate_kbps = parse_positive(&next_arg(&mut args, "--bitrate")?, "--bitrate")?;
        } else if let Some(value) = text.strip_prefix("--fps=") {
            fps = Some(parse_fps(value)?);
        } else if text == "--fps" {
            fps = Some(parse_fps(&next_arg(&mut args, "--fps")?)?);
        } else if let Some(value) = text.strip_prefix("--output-dir=") {
            output_dir = Some(expand_home(Path::new(value)));
        } else if text == "--output-dir" {
            output_dir = Some(expand_home(Path::new(&next_arg(
                &mut args,
                "--output-dir",
            )?)));
        } else if text.starts_with('-') {
            return Err(boxed(format!("unknown option {text}")));
        } else {
            positionals.push(expand_home(Path::new(arg.as_os_str())));
        }
    }

    let mut range = None;
    let mut extract_output = None;
    let inputs = if extract_telemetry {
        match positionals.as_slice() {
            [] => vec![expand_home(Path::new(DEFAULT_INPUT_DIR))],
            [input] => vec![input.clone()],
            [input, output] => {
                extract_output = Some(output.clone());
                vec![input.clone()]
            }
            _ => {
                return Err(boxed(
                    "--extract-telemetry accepts one input and an optional output filename",
                ))
            }
        }
    } else if positionals.len() >= 3 {
        if let (Ok(from), Ok(to)) = (parse_time(&positionals[0]), parse_time(&positionals[1])) {
            if to <= from {
                return Err(boxed("<to> must be after <from>"));
            }
            range = Some(TimeRange { from, to });
            positionals[2..].to_vec()
        } else {
            positionals
        }
    } else if positionals.is_empty() {
        vec![expand_home(Path::new(DEFAULT_INPUT_DIR))]
    } else {
        positionals
    };
    if height % 2 != 0 {
        return Err(boxed("--res must be even for H.264 4:2:0 output"));
    }
    Ok(Config {
        inputs,
        output_dir,
        height,
        bitrate_kbps,
        fps,
        range,
        extract_telemetry,
        extract_output,
        overwrite,
        cpu_force,
    })
}

fn next_arg<I>(args: &mut I, option: &str) -> Result<String, Box<dyn Error>>
where
    I: Iterator<Item = std::ffi::OsString>,
{
    args.next()
        .map(|value| value.to_string_lossy().into_owned())
        .ok_or_else(|| boxed(format!("{option} needs a value")))
}

fn parse_positive(value: &str, option: &str) -> Result<u32, Box<dyn Error>> {
    let value = value
        .strip_suffix('k')
        .or_else(|| value.strip_suffix('K'))
        .unwrap_or(value);
    let parsed = value
        .parse::<u32>()
        .map_err(|_| boxed(format!("{option} must be a positive integer")))?;
    if parsed == 0 {
        return Err(boxed(format!("{option} must be positive")));
    }
    Ok(parsed)
}

fn parse_fps(value: &str) -> Result<Rational, Box<dyn Error>> {
    let (numerator, denominator) = if let Some((numerator, denominator)) = value.split_once('/') {
        (
            numerator
                .parse::<u32>()
                .map_err(|_| boxed("--fps numerator is invalid"))?,
            denominator
                .parse::<u32>()
                .map_err(|_| boxed("--fps denominator is invalid"))?,
        )
    } else if let Some((whole, fraction)) = value.split_once('.') {
        let digits = fraction.len().min(6);
        let fraction = &fraction[..digits];
        let denominator = 10u32.pow(digits as u32);
        let whole = whole
            .parse::<u32>()
            .map_err(|_| boxed("--fps is invalid"))?;
        let fraction = if fraction.is_empty() {
            0
        } else {
            fraction
                .parse::<u32>()
                .map_err(|_| boxed("--fps is invalid"))?
        };
        (
            whole.saturating_mul(denominator).saturating_add(fraction),
            denominator,
        )
    } else {
        (
            value
                .parse::<u32>()
                .map_err(|_| boxed("--fps is invalid"))?,
            1,
        )
    };
    if numerator == 0 || denominator == 0 {
        return Err(boxed("--fps must be positive"));
    }
    let divisor = gcd(numerator, denominator);
    Ok(Rational(
        (numerator / divisor) as i32,
        (denominator / divisor) as i32,
    ))
}
fn parse_time(path: &Path) -> Result<f64, Box<dyn Error>> {
    let value = path
        .to_str()
        .ok_or_else(|| boxed("time value is not valid UTF-8"))?;
    let fields = value.split(':').collect::<Vec<_>>();
    let seconds = match fields.as_slice() {
        [seconds] => seconds.parse::<f64>().map_err(|_| boxed("invalid time"))?,
        [minutes, seconds] => {
            let minutes = minutes.parse::<f64>().map_err(|_| boxed("invalid time"))?;
            let seconds = seconds.parse::<f64>().map_err(|_| boxed("invalid time"))?;
            minutes * 60.0 + seconds
        }
        [hours, minutes, seconds] => {
            let hours = hours.parse::<f64>().map_err(|_| boxed("invalid time"))?;
            let minutes = minutes.parse::<f64>().map_err(|_| boxed("invalid time"))?;
            let seconds = seconds.parse::<f64>().map_err(|_| boxed("invalid time"))?;
            hours * 3_600.0 + minutes * 60.0 + seconds
        }
        _ => return Err(boxed("time must be seconds, MM:SS, or HH:MM:SS")),
    };
    if !seconds.is_finite() || seconds < 0.0 {
        return Err(boxed("time must be finite and non-negative"));
    }
    Ok(seconds)
}

fn extract_telemetry_files(config: &Config) -> Result<(), Box<dyn Error>> {
    let inputs = discover_inputs(&config.inputs)?;
    if let Some(output) = &config.extract_output {
        if inputs.len() != 1 {
            return Err(boxed(
                "an explicit --extract-telemetry output requires exactly one input",
            ));
        }
        if let Some(parent) = output
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)?;
        }
    }
    let mut failures = Vec::new();
    for input in inputs {
        let target = config
            .extract_output
            .clone()
            .unwrap_or_else(|| telemetry_sidecar_path(&input));
        if !config.overwrite && target.exists() {
            eprintln!("{}: output exists (use --overwrite)", input.display());
            failures.push(input);
            continue;
        }
        let filename = target.file_name().unwrap_or_default().to_string_lossy();
        let temp = target
            .parent()
            .unwrap_or(Path::new("."))
            .join(format!(".{filename}.tmp"));
        remove_if_exists(&temp)?;
        let result = (|| {
            let source = open(&input)?;
            telemetry_format::write_from_source(&source, &temp)?;
            fs::rename(&temp, &target)?;
            Ok::<(), Box<dyn Error>>(())
        })();
        if let Err(error) = result {
            let _ = remove_if_exists(&temp);
            eprintln!("{}: {error}", input.display());
            failures.push(input);
        } else {
            println!("{} -> {}", input.display(), target.display());
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(boxed(format!("{} file(s) failed", failures.len())))
    }
}

fn gcd(mut left: u32, mut right: u32) -> u32 {
    while right != 0 {
        (left, right) = (right, left % right);
    }
    left.max(1)
}

fn expand_home(path: &Path) -> PathBuf {
    let Some(text) = path.to_str() else {
        return path.to_owned();
    };
    if text == "~" || text.starts_with("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(text.strip_prefix("~/").unwrap_or(""));
        }
    }
    path.to_owned()
}

fn discover_inputs(paths: &[PathBuf]) -> Result<Vec<PathBuf>, Box<dyn Error>> {
    let mut files = Vec::new();
    for path in paths {
        if path.is_dir() {
            for entry in fs::read_dir(path)? {
                let entry = entry?;
                let candidate = entry.path();
                if candidate.is_file() && is_mp4(&candidate) {
                    files.push(candidate);
                }
            }
        } else if path.is_file() && is_mp4(path) {
            files.push(path.clone());
        } else {
            return Err(boxed(format!(
                "input does not exist or is not an MP4: {}",
                path.display()
            )));
        }
    }
    files.sort();
    files.dedup();
    Ok(files)
}

fn is_mp4(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("mp4"))
}

fn print_usage() {
    println!(
        "Usage: ffmpeg-aimd [OPTIONS] [FROM TO] FILE_OR_DIR ...\n\n\
         Defaults: ~/Documents/upload -> ./reduced, 720p, 3000 kb/s, source FPS\n\n\
         Options:\n\
           --res N             output height (default 720; must be even)\n\
           --bitrate N[k]      H.264 bitrate in kb/s (default 3000)\n\
           --fps N[.N|/N]      output FPS (default keeps source rate)\n\
           --output-dir DIR    output directory (default reduced beside input)\n\
           --extract-telemetry write native .telemetry; use FILE [OUTPUT]\n\
           --cpu               force CPU libx264 instead of hardware encoding\n\
           --overwrite         replace existing output and telemetry sidecar\n\
         FROM and TO are seconds, MM:SS, or HH:MM:SS. The output selects\n\
         the best available VAAPI H.264/HEVC encoder.\n\
         AIMD is retained by atom-level MP4 track splicing and exported as\n\
         hidden .<output>.telemetry sidecar."
    );
}

fn process_one(
    input: &Path,
    output_dir: &Path,
    config: &Config,
) -> Result<PathBuf, Box<dyn Error>> {
    let output = output_path(input, output_dir, config.height);
    let sidecar = telemetry_sidecar_path(&output);
    if !config.overwrite {
        for path in [&output, &sidecar] {
            if path.exists() {
                return Err(boxed(format!(
                    "output exists (use --overwrite): {}",
                    path.display()
                )));
            }
        }
    }

    let temp_video = sibling_temp(&output, "video");
    let temp_mp4 = sibling_temp(&output, "spliced");
    let temp_telemetry = sidecar.with_file_name(format!(
        ".{}.tmp",
        sidecar.file_name().unwrap().to_string_lossy()
    ));
    remove_if_exists(&temp_video)?;
    remove_if_exists(&temp_mp4)?;
    remove_if_exists(&temp_telemetry)?;

    let result = (|| {
        let telemetry = open(input)?;
        transcode(input, &temp_video, config)
            .map_err(|error| boxed(format!("transcode: {error}")))?;
        splice_aimd_track(input, &temp_video, &temp_mp4)
            .map_err(|error| boxed(format!("splice AIMD: {error}")))?;
        telemetry_format::write_from_source(&telemetry, &temp_telemetry)
            .map_err(|error| boxed(format!("write native telemetry: {error}")))?;
        fs::rename(&temp_mp4, &output)?;
        fs::rename(&temp_telemetry, &sidecar)?;
        Ok::<(), Box<dyn Error>>(())
    })();

    if result.is_err() {
        remove_if_exists(&temp_video)?;
        remove_if_exists(&temp_mp4)?;
        remove_if_exists(&temp_telemetry)?;
    }
    result.map(|()| output)
}

fn output_path(input: &Path, output_dir: &Path, height: u32) -> PathBuf {
    let stem = input
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("output");
    output_dir.join(format!("{stem}_{height}p.mp4"))
}

fn telemetry_sidecar_path(output: &Path) -> PathBuf {
    let directory = output.parent().unwrap_or(Path::new("."));
    let filename = output.file_name().unwrap().to_string_lossy();
    directory.join(format!(".{filename}.telemetry"))
}

fn sibling_temp(path: &Path, tag: &str) -> PathBuf {
    let directory = path.parent().unwrap_or(Path::new("."));
    let filename = path.file_name().unwrap().to_string_lossy();
    directory.join(format!(".{filename}.tmp-{tag}.mp4"))
}

fn remove_if_exists(path: &Path) -> Result<(), Box<dyn Error>> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}
fn select_hardware_encoder() -> Option<(&'static str, format::Pixel)> {
    [
        ("hevc_vaapi", format::Pixel::VAAPI),
        ("h264_vaapi", format::Pixel::VAAPI),
    ]
    .into_iter()
    .find(|(name, _)| ffmpeg::encoder::find_by_name(name).is_some())
}
fn transcode(input: &Path, output: &Path, config: &Config) -> Result<(), Box<dyn Error>> {
    let Some((encoder, pixel_format)) = select_hardware_encoder() else {
        if config.cpu_force {
            eprintln!("no hardware encoder; using libx264 because --cpu was given");
            return transcode_with_encoder(
                input,
                output,
                config,
                "libx264",
                format::Pixel::YUV420P,
            );
        }
        return Err(boxed(
            "no supported VAAPI H.264/HEVC hardware encoder found",
        ));
    };
    eprintln!("selected hardware encoder {encoder}");
    match transcode_with_encoder(input, output, config, encoder, pixel_format) {
        Ok(()) => Ok(()),
        Err(error) if config.cpu_force => {
            eprintln!("{encoder} failed ({error}); using libx264 because --cpu was given");
            remove_if_exists(output)?;
            transcode_with_encoder(input, output, config, "libx264", format::Pixel::YUV420P)
        }
        Err(error) => Err(error),
    }
}

fn transcode_with_encoder(
    input: &Path,
    output: &Path,
    config: &Config,
    encoder_name: &str,
    pixel_format: format::Pixel,
) -> Result<(), Box<dyn Error>> {
    let mut ictx = format::input(input)?;
    let stream_info = ictx
        .streams()
        .map(|stream| InputStream {
            index: stream.index(),
            kind: stream.parameters().medium(),
            time_base: stream.time_base(),
            parameters: stream.parameters(),
            frames: stream.frames(),
        })
        .collect::<Vec<_>>();
    let video = stream_info
        .iter()
        .find(|stream| stream.kind == media::Type::Video)
        .ok_or_else(|| boxed("input has no video stream"))?;
    let video_index = video.index;
    let video_context = codec::context::Context::from_parameters(video.parameters.clone())?;
    let mut decoder = video_context.decoder().video()?;
    let source_fps = source_fps(&ictx, video_index);
    let fps = config.fps.unwrap_or(source_fps);
    let output_height = config.height;
    let output_width = scaled_width(decoder.width(), decoder.height(), output_height);
    let output_time_base = Rational(fps.1, fps.0);
    let encoder_codec = ffmpeg::encoder::find_by_name(encoder_name)
        .ok_or_else(|| boxed(format!("FFmpeg encoder {encoder_name} is unavailable")))?;

    let mut octx = format::output(output)?;
    let mut copy_streams = Vec::new();
    let mut video_output_index = None;
    for stream in &stream_info {
        if stream.index == video_index {
            let output_stream = octx.add_stream(encoder_codec.clone())?;
            video_output_index = Some(output_stream.index());
        } else if stream.kind != media::Type::Data {
            let mut output_stream = octx.add_stream(None::<ffmpeg::Codec>)?;
            output_stream.set_parameters(stream.parameters.clone());
            output_stream.set_time_base(stream.time_base);
            copy_streams.push(CopyStream {
                input_index: stream.index,
                output_index: output_stream.index(),
                input_time_base: stream.time_base,
                output_time_base: stream.time_base,
            });
        }
    }
    let video_output_index =
        video_output_index.ok_or_else(|| boxed("failed to create video stream"))?;
    let global_header = octx
        .format()
        .flags()
        .contains(format::flag::Flags::GLOBAL_HEADER);
    let hw_device = if encoder_name.ends_with("_vaapi") {
        Some(create_hw_device()?)
    } else {
        None
    };
    let hw_frames = hw_device
        .map(|device| create_hw_frames(device, output_width, output_height))
        .transpose()?;
    let mut encoder = codec::context::Context::new_with_codec(encoder_codec.clone())
        .encoder()
        .video()?;
    encoder.set_width(output_width);
    encoder.set_height(output_height);
    encoder.set_format(pixel_format);
    encoder.set_bit_rate(config.bitrate_kbps as usize * 1_000);
    encoder.set_time_base(output_time_base);
    encoder.set_frame_rate(Some(fps));
    if let Some(device) = hw_device {
        unsafe {
            (*encoder.as_mut_ptr()).hw_device_ctx = ffmpeg::ffi::av_buffer_ref(device);
        }
    }
    if let Some(frames) = &hw_frames {
        unsafe {
            (*encoder.as_mut_ptr()).hw_frames_ctx = ffmpeg::ffi::av_buffer_ref(frames.frames);
        }
    }
    if global_header {
        encoder.set_flags(codec::flag::Flags::GLOBAL_HEADER);
    }
    let mut encoder = encoder.open_as(encoder_codec)?;
    {
        let mut output_stream = octx
            .stream_mut(video_output_index)
            .ok_or_else(|| boxed("failed to access output video stream"))?;
        output_stream.set_parameters(&encoder);
        output_stream.set_time_base(output_time_base);
        output_stream.set_rate(fps);
        output_stream.set_avg_frame_rate(fps);
    }

    let mut filter = build_video_filter(
        &decoder,
        output_width,
        output_height,
        fps,
        video.time_base,
        pixel_format,
        config.range,
        hw_device,
    )?;
    octx.set_metadata(ictx.metadata().to_owned());
    octx.write_header()?;
    let mux_time_base = octx
        .stream(video_output_index)
        .ok_or_else(|| boxed("failed to access muxed video stream"))?
        .time_base();

    let mut decoded = frame::Video::empty();
    let mut next_video_pts = 0i64;
    let total_video_frames = u64::try_from(video.frames.max(0)).ok();
    let progress_start = Instant::now();
    let mut last_progress = Instant::now() - Duration::from_secs(2);
    for (stream, mut packet) in ictx.packets() {
        if stream.index() == video_index {
            if let Some(range) = config.range {
                if packet_start_seconds(&packet, stream.time_base()) >= range.to {
                    break;
                }
            }
            packet.rescale_ts(stream.time_base(), video.time_base);
            decoder.send_packet(&packet)?;
            drain_decoder(
                &mut decoder,
                &mut decoded,
                &mut filter,
                &mut encoder,
                &mut octx,
                video_output_index,
                mux_time_base,
                &mut next_video_pts,
            )?;
            report_progress(
                u64::try_from(next_video_pts.max(0)).unwrap_or(0),
                total_video_frames,
                progress_start,
                &mut last_progress,
                false,
            );
        } else if let Some(copy) = copy_streams
            .iter()
            .find(|copy| copy.input_index == stream.index())
        {
            if let Some(range) = config.range {
                if !packet_overlaps_range(&packet, copy.input_time_base, range) {
                    continue;
                }
            }
            packet.rescale_ts(copy.input_time_base, copy.output_time_base);
            if let Some(range) = config.range {
                shift_packet_to_zero(&mut packet, copy.output_time_base, range.from);
            }
            packet.set_stream(copy.output_index);
            packet.write_interleaved(&mut octx)?;
        }
    }

    decoder.send_eof()?;
    drain_decoder(
        &mut decoder,
        &mut decoded,
        &mut filter,
        &mut encoder,
        &mut octx,
        video_output_index,
        mux_time_base,
        &mut next_video_pts,
    )?;
    filter
        .get("in")
        .ok_or_else(|| boxed("video filter has no input"))?
        .source()
        .flush()?;
    drain_filtered(
        &mut filter,
        &mut encoder,
        &mut octx,
        video_output_index,
        mux_time_base,
        &mut next_video_pts,
    )?;
    encoder.send_eof()?;
    drain_encoder(&mut encoder, &mut octx, video_output_index, mux_time_base)?;
    octx.write_trailer()?;
    if let Some(frames) = hw_frames {
        unsafe {
            ffmpeg::ffi::av_buffer_unref(&mut (frames.frames as *mut _));
        }
    }
    if let Some(mut device) = hw_device {
        unsafe {
            ffmpeg::ffi::av_buffer_unref(&mut device);
        }
    }
    report_progress(
        u64::try_from(next_video_pts.max(0)).unwrap_or(0),
        total_video_frames,
        progress_start,
        &mut last_progress,
        true,
    );
    println!();
    Ok(())
}

fn report_progress(
    processed: u64,
    total: Option<u64>,
    started: Instant,
    last_report: &mut Instant,
    force: bool,
) {
    if !force && last_report.elapsed() < Duration::from_secs(1) {
        return;
    }
    let elapsed = started.elapsed().as_secs_f64();
    let (percent, eta) = if let Some(total) = total.filter(|total| *total > 0) {
        let fraction = (processed as f64 / total as f64).min(1.0);
        let eta = if fraction > 0.0 {
            elapsed * (1.0 - fraction) / fraction
        } else {
            0.0
        };
        (fraction * 100.0, eta)
    } else {
        (0.0, 0.0)
    };
    print!("\rframes={processed} progress={percent:5.1}% elapsed={elapsed:7.1}s eta={eta:7.1}s");
    let _ = io::stdout().flush();
    *last_report = Instant::now();
}

fn source_fps(ictx: &format::context::Input, video_index: usize) -> Rational {
    let stream = ictx.stream(video_index).expect("video stream disappeared");
    let fps = stream.avg_frame_rate();
    if fps.0 > 0 && fps.1 > 0 {
        fps
    } else {
        let rate = stream.rate();
        if rate.0 > 0 && rate.1 > 0 {
            rate
        } else {
            Rational(30, 1)
        }
    }
}

fn scaled_width(width: u32, height: u32, output_height: u32) -> u32 {
    let scaled = (u64::from(width) * u64::from(output_height) + u64::from(height / 2))
        / u64::from(height.max(1));
    (scaled.max(2) as u32) & !1
}

fn create_hw_device() -> Result<*mut ffmpeg::ffi::AVBufferRef, Box<dyn Error>> {
    let name = CString::new("vaapi")?;
    let device_type = unsafe { ffmpeg::ffi::av_hwdevice_find_type_by_name(name.as_ptr()) };
    let mut candidates = fs::read_dir("/dev/dri")?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("renderD"))
        })
        .collect::<Vec<_>>();
    candidates.sort();
    let mut last_error = None;
    for candidate in candidates {
        let device_name = CString::new(candidate.to_string_lossy().as_bytes())?;
        let mut device = ptr::null_mut();
        let result = unsafe {
            ffmpeg::ffi::av_hwdevice_ctx_create(
                &mut device,
                device_type,
                device_name.as_ptr(),
                ptr::null_mut(),
                0,
            )
        };
        if result >= 0 && !device.is_null() {
            eprintln!("using VAAPI device {}", candidate.display());
            return Ok(device);
        }
        last_error = Some(format!(
            "{}: {}",
            candidate.display(),
            ffmpeg::Error::from(result)
        ));
    }
    Err(boxed(format!(
        "initialising VAAPI device failed{}",
        last_error.map_or_else(String::new, |error| format!(": {error}"))
    )))
}

struct QsvFrames {
    frames: *mut ffmpeg::ffi::AVBufferRef,
}

fn create_hw_frames(
    device: *mut ffmpeg::ffi::AVBufferRef,
    width: u32,
    height: u32,
) -> Result<QsvFrames, Box<dyn Error>> {
    let frames = unsafe { ffmpeg::ffi::av_hwframe_ctx_alloc(ffmpeg::ffi::av_buffer_ref(device)) };
    if frames.is_null() {
        return Err(boxed("allocating hardware frame context failed"));
    }
    unsafe {
        let context = (*frames).data as *mut ffmpeg::ffi::AVHWFramesContext;
        (*context).format = ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_VAAPI;
        (*context).sw_format = ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_NV12;
        (*context).width = width as i32;
        (*context).height = height as i32;
        (*context).initial_pool_size = 64;
        let result = ffmpeg::ffi::av_hwframe_ctx_init(frames);
        if result < 0 {
            ffmpeg::ffi::av_buffer_unref(&mut (frames as *mut _));
            return Err(boxed(format!(
                "initialising hardware frame context: {}",
                ffmpeg::Error::from(result)
            )));
        }
    }
    Ok(QsvFrames { frames })
}

fn add_hwupload(
    graph: &mut ffmpeg::filter::Graph,
    device: *mut ffmpeg::ffi::AVBufferRef,
) -> Result<ffmpeg::filter::Context, Box<dyn Error>> {
    let filter_name = CString::new("hwupload")?;
    let instance_name = CString::new("upload")?;
    let options = CString::new("extra_hw_frames=64")?;
    let filter = unsafe { ffmpeg::ffi::avfilter_get_by_name(filter_name.as_ptr()) };
    let context = unsafe {
        ffmpeg::ffi::avfilter_graph_alloc_filter(graph.as_mut_ptr(), filter, instance_name.as_ptr())
    };
    if context.is_null() {
        return Err(boxed("allocating hwupload filter failed"));
    }
    unsafe {
        (*context).hw_device_ctx = ffmpeg::ffi::av_buffer_ref(device);
        let result = ffmpeg::ffi::avfilter_init_str(context, options.as_ptr());
        if result < 0 {
            return Err(boxed(format!(
                "initialising hwupload filter: {}",
                ffmpeg::Error::from(result)
            )));
        }
        Ok(ffmpeg::filter::Context::wrap(context))
    }
}
fn build_video_filter(
    decoder: &codec::decoder::Video,
    width: u32,
    height: u32,
    fps: Rational,
    time_base: Rational,
    output_format: format::Pixel,
    range: Option<TimeRange>,
    qsv_device: Option<*mut ffmpeg::ffi::AVBufferRef>,
) -> Result<ffmpeg::filter::Graph, Box<dyn Error>> {
    let mut graph = ffmpeg::filter::Graph::new();
    let pixel_aspect = decoder.aspect_ratio();
    let pixel_format = decoder
        .format()
        .descriptor()
        .ok_or_else(|| boxed("decoder returned an unknown pixel format"))?
        .name();
    let args = format!(
        "video_size={}x{}:pix_fmt={}:time_base={}:pixel_aspect={}",
        decoder.width(),
        decoder.height(),
        pixel_format,
        time_base,
        pixel_aspect
    );
    let mut source = graph.add(
        &ffmpeg::filter::find("buffer").ok_or_else(|| boxed("FFmpeg buffer filter missing"))?,
        "in",
        &args,
    )?;
    let mut scale = graph.add(
        &ffmpeg::filter::find("scale").ok_or_else(|| boxed("FFmpeg scale filter missing"))?,
        "scale",
        &format!("w={width}:h={height}:flags=fast_bilinear"),
    )?;
    if let Some(range) = range {
        let mut trim = graph.add(
            &ffmpeg::filter::find("trim").ok_or_else(|| boxed("FFmpeg trim filter missing"))?,
            "trim",
            &format!("start={:.6}:end={:.6}", range.from, range.to),
        )?;
        let mut setpts = graph.add(
            &ffmpeg::filter::find("setpts").ok_or_else(|| boxed("FFmpeg setpts filter missing"))?,
            "setpts",
            "expr=PTS-STARTPTS",
        )?;
        source.link(0, &mut trim, 0);
        trim.link(0, &mut setpts, 0);
        setpts.link(0, &mut scale, 0);
    } else {
        source.link(0, &mut scale, 0);
    }
    let mut fps_filter = graph.add(
        &ffmpeg::filter::find("fps").ok_or_else(|| boxed("FFmpeg fps filter missing"))?,
        "fps",
        &format!("fps={}/{}", fps.0, fps.1),
    )?;
    scale.link(0, &mut fps_filter, 0);
    let mut sink = graph.add(
        &ffmpeg::filter::find("buffersink")
            .ok_or_else(|| boxed("FFmpeg buffersink filter missing"))?,
        "out",
        "",
    )?;
    if let Some(device) = qsv_device {
        let mut format_in = graph.add(
            &ffmpeg::filter::find("format").ok_or_else(|| boxed("FFmpeg format filter missing"))?,
            "format_in",
            "pix_fmts=nv12",
        )?;
        fps_filter.link(0, &mut format_in, 0);
        let mut upload = add_hwupload(&mut graph, device)?;
        let mut hardware_format = graph.add(
            &ffmpeg::filter::find("format")
                .ok_or_else(|| boxed("FFmpeg hardware format filter missing"))?,
            "format_vaapi",
            "pix_fmts=vaapi",
        )?;
        format_in.link(0, &mut upload, 0);
        upload.link(0, &mut hardware_format, 0);
        hardware_format.link(0, &mut sink, 0);
        sink.set_pixel_format(format::Pixel::VAAPI);
    } else {
        let output_name = output_format
            .descriptor()
            .ok_or_else(|| boxed("encoder output format has no descriptor"))?
            .name();
        let mut format_out = graph.add(
            &ffmpeg::filter::find("format")
                .ok_or_else(|| boxed("FFmpeg output format filter missing"))?,
            "format_out",
            &format!("pix_fmts={output_name}"),
        )?;
        fps_filter.link(0, &mut format_out, 0);
        format_out.link(0, &mut sink, 0);
        sink.set_pixel_format(output_format);
    }
    graph.validate()?;
    Ok(graph)
}
fn packet_start_seconds(packet: &ffmpeg::Packet, time_base: Rational) -> f64 {
    packet.pts().or_else(|| packet.dts()).map_or(0.0, |pts| {
        pts as f64 * f64::from(time_base.0) / f64::from(time_base.1.max(1))
    })
}

fn packet_overlaps_range(packet: &ffmpeg::Packet, time_base: Rational, range: TimeRange) -> bool {
    let scale = f64::from(time_base.0) / f64::from(time_base.1.max(1));
    let start = packet
        .pts()
        .or_else(|| packet.dts())
        .map_or(0.0, |pts| pts as f64 * scale);
    let duration = packet.duration().max(0) as f64 * scale;
    if duration > 0.0 {
        start < range.to && start + duration > range.from
    } else {
        start >= range.from && start < range.to
    }
}

fn shift_packet_to_zero(packet: &mut ffmpeg::Packet, time_base: Rational, from_seconds: f64) {
    let ticks =
        (from_seconds * f64::from(time_base.1) / f64::from(time_base.0.max(1))).round() as i64;
    if let Some(pts) = packet.pts() {
        packet.set_pts(Some(pts.saturating_sub(ticks)));
    }
    if let Some(dts) = packet.dts() {
        packet.set_dts(Some(dts.saturating_sub(ticks)));
    }
}

fn drain_decoder(
    decoder: &mut codec::decoder::Video,
    decoded: &mut frame::Video,
    filter: &mut ffmpeg::filter::Graph,
    encoder: &mut codec::encoder::video::Encoder,
    output: &mut format::context::Output,
    output_stream: usize,
    output_time_base: Rational,
    next_video_pts: &mut i64,
) -> Result<(), Box<dyn Error>> {
    loop {
        match decoder.receive_frame(decoded) {
            Ok(()) => {
                if decoded.pts().is_none() {
                    let timestamp = decoded.timestamp();
                    decoded.set_pts(timestamp);
                }
                filter
                    .get("in")
                    .ok_or_else(|| boxed("video filter has no input"))?
                    .source()
                    .add(decoded)?;
                drain_filtered(
                    filter,
                    encoder,
                    output,
                    output_stream,
                    output_time_base,
                    next_video_pts,
                )?;
            }
            Err(error) if is_again(&error) || error == ffmpeg::Error::Eof => break,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn drain_filtered(
    filter: &mut ffmpeg::filter::Graph,
    encoder: &mut codec::encoder::video::Encoder,
    output: &mut format::context::Output,
    output_stream: usize,
    output_time_base: Rational,
    next_video_pts: &mut i64,
) -> Result<(), Box<dyn Error>> {
    let mut filtered = frame::Video::empty();
    loop {
        let result = filter
            .get("out")
            .ok_or_else(|| boxed("video filter has no output"))?
            .sink()
            .frame(&mut filtered);
        match result {
            Ok(()) => {
                filtered.set_pts(Some(*next_video_pts));
                *next_video_pts += 1;
                encoder.send_frame(&filtered)?;
                drain_encoder(encoder, output, output_stream, output_time_base)?;
            }
            Err(error) if is_again(&error) || error == ffmpeg::Error::Eof => break,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn drain_encoder(
    encoder: &mut codec::encoder::video::Encoder,
    output: &mut format::context::Output,
    output_stream: usize,
    output_time_base: Rational,
) -> Result<(), Box<dyn Error>> {
    let mut packet = ffmpeg::Packet::empty();
    loop {
        match encoder.receive_packet(&mut packet) {
            Ok(()) => {
                packet.set_stream(output_stream);
                packet.rescale_ts(encoder.time_base(), output_time_base);
                packet.write_interleaved(output)?;
            }
            Err(error) if is_again(&error) || error == ffmpeg::Error::Eof => break,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn is_again(error: &ffmpeg::Error) -> bool {
    matches!(error, ffmpeg::Error::Other { errno } if *errno == ffmpeg::error::EAGAIN)
}

fn map_file(path: &Path) -> Result<Mmap, Box<dyn Error>> {
    let file = File::open(path)?;
    // The mapping is read-only and lives no longer than the returned file view.
    Ok(unsafe { MmapOptions::new().map(&file)? })
}

fn splice_aimd_track(
    source_path: &Path,
    transcoded_path: &Path,
    output_path: &Path,
) -> Result<(), Box<dyn Error>> {
    let source = map_file(source_path)?;
    let transcoded = fs::read(transcoded_path)?;
    let source_top = parse_top_until_moov(&source)?;
    let source_moov = source_top
        .iter()
        .find(|atom| atom.kind == *b"moov")
        .ok_or_else(|| boxed("source MP4 has no moov atom"))?;
    let aimd = find_aimd_track(&source, *source_moov)?;
    let output_top = parse_atoms(&transcoded, 0, transcoded.len())
        .map_err(|error| boxed(format!("parsing FFmpeg output atoms: {error}")))?;
    let output_moov = output_top
        .iter()
        .find(|atom| atom.kind == *b"moov")
        .ok_or_else(|| boxed("FFmpeg output has no moov atom"))?;
    if output_moov.end != transcoded.len() {
        return Err(boxed(
            "FFmpeg output moov is not the final atom; cannot safely splice AIMD offsets",
        ));
    }

    let mut patched_track = source[aimd.atom.start..aimd.atom.end].to_vec();
    let existing_ids = track_ids(&transcoded, *output_moov)?;
    let new_track_id = existing_ids
        .into_iter()
        .max()
        .unwrap_or(0)
        .saturating_add(1);
    patch_track_id(&mut patched_track, new_track_id)?;

    let payload_size = aimd
        .chunks
        .iter()
        .try_fold(0usize, |sum, (_, size)| sum.checked_add(*size))
        .ok_or_else(|| boxed("AIMD payload size overflow"))?;
    let mdat_size = payload_size
        .checked_add(8)
        .ok_or_else(|| boxed("AIMD mdat size overflow"))?;
    if mdat_size > u32::MAX as usize {
        return Err(boxed("AIMD payload exceeds 4 GiB MP4 atom limit"));
    }
    let payload_start = output_moov
        .start
        .checked_add(8)
        .ok_or_else(|| boxed("MP4 offset overflow"))?;
    let mut new_offsets = Vec::with_capacity(aimd.chunks.len());
    let mut aimd_payload = Vec::with_capacity(payload_size);
    let mut destination_offset = payload_start;
    for (source_offset, size) in &aimd.chunks {
        let end = source_offset
            .checked_add(*size)
            .ok_or_else(|| boxed("source AIMD chunk offset overflow"))?;
        let bytes = source
            .get(*source_offset..end)
            .ok_or_else(|| boxed("source AIMD chunk lies outside MP4"))?;
        new_offsets.push(destination_offset);
        aimd_payload.extend_from_slice(bytes);
        destination_offset = destination_offset
            .checked_add(*size)
            .ok_or_else(|| boxed("destination AIMD offset overflow"))?;
    }
    patch_chunk_offsets(&mut patched_track, &new_offsets)?;
    let mut patched_moov = transcoded[output_moov.start..output_moov.end].to_vec();
    append_track(&mut patched_moov, &patched_track)?;
    let mut mdat = Vec::with_capacity(mdat_size);
    mdat.extend_from_slice(&(mdat_size as u32).to_be_bytes());
    mdat.extend_from_slice(b"mdat");
    mdat.extend_from_slice(&aimd_payload);

    let mut result = Vec::with_capacity(transcoded.len() + mdat.len() + patched_track.len());
    result.extend_from_slice(&transcoded[..output_moov.start]);
    result.extend_from_slice(&mdat);
    result.extend_from_slice(&patched_moov);
    fs::write(output_path, result)?;
    Ok(())
}

fn parse_atoms(data: &[u8], start: usize, end: usize) -> Result<Vec<Atom>, Box<dyn Error>> {
    let mut atoms = Vec::new();
    let mut cursor = start;
    while cursor < end {
        if end - cursor < 8 {
            return Err(boxed("truncated MP4 atom header"));
        }
        let size32 = read_u32(data, cursor)? as u64;
        let kind = data[cursor + 4..cursor + 8]
            .try_into()
            .map_err(|_| boxed("invalid MP4 atom type"))?;
        let (header, size) = if size32 == 1 {
            if end - cursor < 16 {
                return Err(boxed("truncated extended MP4 atom header"));
            }
            (16usize, read_u64(data, cursor + 8)?)
        } else if size32 == 0 {
            (8usize, (end - cursor) as u64)
        } else {
            (8usize, size32)
        };
        let size = usize::try_from(size).map_err(|_| boxed("MP4 atom is too large"))?;
        let atom_end = cursor
            .checked_add(size)
            .ok_or_else(|| boxed("MP4 atom end overflow"))?;
        if size < header || atom_end > end {
            return Err(boxed(format!(
                "invalid MP4 atom size at {cursor} ({}) size={size} header={header} limit={end}",
                printable(kind)
            )));
        }
        atoms.push(Atom {
            start: cursor,
            payload: cursor + header,
            end: atom_end,
            kind,
        });
        cursor = atom_end;
    }
    Ok(atoms)
}

fn parse_top_until_moov(data: &[u8]) -> Result<Vec<Atom>, Box<dyn Error>> {
    let mut atoms = Vec::new();
    let mut cursor = 0;
    while cursor + 8 <= data.len() {
        let size32 = read_u32(data, cursor)? as u64;
        let kind = data[cursor + 4..cursor + 8]
            .try_into()
            .map_err(|_| boxed("invalid MP4 atom type"))?;
        let (header, size) = if size32 == 1 {
            if cursor + 16 > data.len() {
                break;
            }
            (16usize, read_u64(data, cursor + 8)?)
        } else if size32 == 0 {
            (8usize, (data.len() - cursor) as u64)
        } else {
            (8usize, size32)
        };
        let Ok(size) = usize::try_from(size) else {
            break;
        };
        let Some(end) = cursor.checked_add(size) else {
            break;
        };
        if size < header || end > data.len() {
            break;
        }
        atoms.push(Atom {
            start: cursor,
            payload: cursor + header,
            end,
            kind,
        });
        cursor = end;
        if kind == *b"moov" {
            return Ok(atoms);
        }
    }
    Err(boxed("source MP4 has no complete moov atom"))
}

fn child(data: &[u8], parent: Atom, kind: &[u8; 4]) -> Result<Atom, Box<dyn Error>> {
    parse_atoms(data, parent.payload, parent.end)?
        .into_iter()
        .find(|atom| &atom.kind == kind)
        .ok_or_else(|| {
            boxed(format!(
                "MP4 atom {} has no {} child",
                printable(parent.kind),
                printable(*kind)
            ))
        })
}

fn printable(kind: [u8; 4]) -> String {
    String::from_utf8_lossy(&kind).into_owned()
}

fn find_aimd_track(data: &[u8], moov: Atom) -> Result<AimdTrack, Box<dyn Error>> {
    for track in parse_atoms(data, moov.payload, moov.end)?
        .into_iter()
        .filter(|atom| atom.kind == *b"trak")
    {
        let mdia = child(data, track, b"mdia")?;
        let minf = child(data, mdia, b"minf")?;
        let stbl = child(data, minf, b"stbl")?;
        let stsd = child(data, stbl, b"stsd")?;
        if stsd_is_aimd(data, stsd)? {
            return Ok(AimdTrack {
                atom: track,
                chunks: track_chunks(data, stbl)?,
            });
        }
    }
    Err(boxed("source MP4 has no AIMD track"))
}

fn stsd_is_aimd(data: &[u8], stsd: Atom) -> Result<bool, Box<dyn Error>> {
    let entry_count = read_u32(data, stsd.payload + 4)? as usize;
    let mut cursor = stsd.payload + 8;
    for _ in 0..entry_count {
        let size = read_u32(data, cursor)? as usize;
        if size < 8 || cursor.checked_add(size).is_none_or(|end| end > stsd.end) {
            return Err(boxed("invalid MP4 stsd entry"));
        }
        if &data[cursor + 4..cursor + 8] == b"aimd" {
            return Ok(true);
        }
        cursor += size;
    }
    Ok(false)
}

fn track_chunks(data: &[u8], stbl: Atom) -> Result<Vec<(usize, usize)>, Box<dyn Error>> {
    let stsc = child(data, stbl, b"stsc")?;
    let stsz = child(data, stbl, b"stsz")?;
    let offsets_atom = parse_atoms(data, stbl.payload, stbl.end)?
        .into_iter()
        .find(|atom| atom.kind == *b"stco" || atom.kind == *b"co64")
        .ok_or_else(|| boxed("AIMD track has no chunk offsets"))?;
    let chunk_count = read_u32(data, offsets_atom.payload + 4)? as usize;
    let mut offsets = Vec::with_capacity(chunk_count);
    for index in 0..chunk_count {
        let offset = if offsets_atom.kind == *b"stco" {
            u64::from(read_u32(data, offsets_atom.payload + 8 + index * 4)?)
        } else {
            read_u64(data, offsets_atom.payload + 8 + index * 8)?
        };
        offsets.push(usize::try_from(offset).map_err(|_| boxed("AIMD chunk offset is too large"))?);
    }

    let stsc_count = read_u32(data, stsc.payload + 4)? as usize;
    let mut stsc_entries = Vec::with_capacity(stsc_count);
    for index in 0..stsc_count {
        let at = stsc.payload + 8 + index * 12;
        stsc_entries.push((read_u32(data, at)?, read_u32(data, at + 4)?));
    }
    if stsc_entries.is_empty() {
        return Err(boxed("AIMD track has no stsc entries"));
    }
    let default_size = read_u32(data, stsz.payload + 4)?;
    let sample_count = read_u32(data, stsz.payload + 8)? as usize;
    let mut sample_sizes = Vec::new();
    if default_size == 0 {
        sample_sizes.reserve(sample_count);
        for index in 0..sample_count {
            sample_sizes.push(read_u32(data, stsz.payload + 12 + index * 4)? as usize);
        }
    }

    let mut chunks = Vec::with_capacity(offsets.len());
    let mut sample_index = 0usize;
    for (chunk_index, offset) in offsets.into_iter().enumerate() {
        let chunk_number = (chunk_index + 1) as u32;
        let entry = stsc_entries
            .iter()
            .enumerate()
            .rev()
            .find(|(_, (first_chunk, _))| *first_chunk <= chunk_number)
            .ok_or_else(|| boxed("AIMD stsc does not cover all chunks"))?
            .1;
        let samples_per_chunk =
            usize::try_from(entry.1).map_err(|_| boxed("AIMD sample count overflow"))?;
        let size = if default_size != 0 {
            samples_per_chunk
                .checked_mul(default_size as usize)
                .ok_or_else(|| boxed("AIMD chunk size overflow"))?
        } else {
            let end = sample_index
                .checked_add(samples_per_chunk)
                .ok_or_else(|| boxed("AIMD sample index overflow"))?;
            let size = sample_sizes
                .get(sample_index..end)
                .ok_or_else(|| boxed("AIMD stsc/stsz sample counts disagree"))?
                .iter()
                .try_fold(0usize, |sum, size| sum.checked_add(*size))
                .ok_or_else(|| boxed("AIMD chunk size overflow"))?;
            sample_index = end;
            size
        };
        chunks.push((offset, size));
    }
    if default_size != 0 {
        let expected = chunks.iter().try_fold(0usize, |sum, (_, size)| {
            sum.checked_add(size / default_size as usize)
        });
        if expected != Some(sample_count) {
            return Err(boxed("AIMD stsc/stsz sample counts disagree"));
        }
    } else if sample_index != sample_count {
        return Err(boxed("AIMD stsc/stsz sample counts disagree"));
    }
    Ok(chunks)
}

fn track_ids(data: &[u8], moov: Atom) -> Result<Vec<u32>, Box<dyn Error>> {
    let mut ids = Vec::new();
    for track in parse_atoms(data, moov.payload, moov.end)?
        .into_iter()
        .filter(|atom| atom.kind == *b"trak")
    {
        if let Ok(tkhd) = child(data, track, b"tkhd") {
            let version = data[tkhd.payload];
            let offset = if version == 1 { 20 } else { 12 };
            ids.push(read_u32(data, tkhd.payload + offset)?);
        }
    }
    Ok(ids)
}

fn patch_track_id(track: &mut [u8], id: u32) -> Result<(), Box<dyn Error>> {
    let root = Atom {
        start: 0,
        payload: atom_header_len(track)?,
        end: track.len(),
        kind: *b"trak",
    };
    let tkhd = child(track, root, b"tkhd")?;
    let version = track[tkhd.payload];
    let offset = if version == 1 { 20 } else { 12 };
    write_u32(track, tkhd.payload + offset, id)
}

fn patch_chunk_offsets(track: &mut [u8], offsets: &[usize]) -> Result<(), Box<dyn Error>> {
    let root = Atom {
        start: 0,
        payload: atom_header_len(track)?,
        end: track.len(),
        kind: *b"trak",
    };
    let mdia = child(track, root, b"mdia")?;
    let minf = child(track, mdia, b"minf")?;
    let stbl = child(track, minf, b"stbl")?;
    let offset_atoms = parse_atoms(track, stbl.payload, stbl.end)?
        .into_iter()
        .filter(|atom| atom.kind == *b"stco" || atom.kind == *b"co64")
        .collect::<Vec<_>>();
    if offset_atoms.len() != 1 {
        return Err(boxed(
            "AIMD track has an unsupported number of chunk offset tables",
        ));
    }
    let offsets_atom = offset_atoms[0];
    let count = read_u32(track, offsets_atom.payload + 4)? as usize;
    if count != offsets.len() {
        return Err(boxed("AIMD chunk offset count changed during splicing"));
    }
    for (index, offset) in offsets.iter().enumerate() {
        if offsets_atom.kind == *b"stco" {
            let offset = u32::try_from(*offset)
                .map_err(|_| boxed("spliced AIMD offset exceeds stco range"))?;
            write_u32(track, offsets_atom.payload + 8 + index * 4, offset)?;
        } else {
            write_u64(track, offsets_atom.payload + 8 + index * 8, *offset as u64)?;
        }
    }
    Ok(())
}

fn append_track(moov: &mut Vec<u8>, track: &[u8]) -> Result<(), Box<dyn Error>> {
    let header = atom_header_len(moov)?;
    moov.extend_from_slice(track);
    let size = u32::try_from(moov.len()).map_err(|_| boxed("output moov exceeds 4 GiB"))?;
    write_u32(moov, 0, size)?;
    if header != 8 {
        return Err(boxed("extended-size output moov is unsupported"));
    }
    Ok(())
}

fn atom_header_len(data: &[u8]) -> Result<usize, Box<dyn Error>> {
    if data.len() < 8 {
        return Err(boxed("truncated atom"));
    }
    Ok(if read_u32(data, 0)? == 1 { 16 } else { 8 })
}

fn read_u32(data: &[u8], offset: usize) -> Result<u32, Box<dyn Error>> {
    let bytes = data
        .get(offset..offset + 4)
        .ok_or_else(|| boxed("truncated MP4 integer"))?;
    Ok(u32::from_be_bytes(bytes.try_into().unwrap()))
}

fn read_u64(data: &[u8], offset: usize) -> Result<u64, Box<dyn Error>> {
    let bytes = data
        .get(offset..offset + 8)
        .ok_or_else(|| boxed("truncated MP4 integer"))?;
    Ok(u64::from_be_bytes(bytes.try_into().unwrap()))
}

fn write_u32(data: &mut [u8], offset: usize, value: u32) -> Result<(), Box<dyn Error>> {
    let bytes = data
        .get_mut(offset..offset + 4)
        .ok_or_else(|| boxed("truncated MP4 integer"))?;
    bytes.copy_from_slice(&value.to_be_bytes());
    Ok(())
}

fn write_u64(data: &mut [u8], offset: usize, value: u64) -> Result<(), Box<dyn Error>> {
    let bytes = data
        .get_mut(offset..offset + 8)
        .ok_or_else(|| boxed("truncated MP4 integer"))?;
    bytes.copy_from_slice(&value.to_be_bytes());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn atom(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let size = u32::try_from(payload.len() + 8).unwrap();
        [size.to_be_bytes().as_slice(), kind.as_slice(), payload].concat()
    }

    fn synthetic_aimd_mp4() -> Vec<u8> {
        let mut stsd_payload = vec![0; 8];
        stsd_payload[4..8].copy_from_slice(&1u32.to_be_bytes());
        stsd_payload.extend(atom(b"aimd", &[]));
        let mut stsc_payload = vec![0; 8];
        stsc_payload[4..8].copy_from_slice(&1u32.to_be_bytes());
        stsc_payload.extend(1u32.to_be_bytes());
        stsc_payload.extend(1u32.to_be_bytes());
        stsc_payload.extend(1u32.to_be_bytes());
        let mut stsz_payload = vec![0; 12];
        stsz_payload[8..12].copy_from_slice(&2u32.to_be_bytes());
        stsz_payload.extend(3u32.to_be_bytes());
        stsz_payload.extend(4u32.to_be_bytes());
        let mut stco_payload = vec![0; 8];
        stco_payload[4..8].copy_from_slice(&2u32.to_be_bytes());
        let stsd = atom(b"stsd", &stsd_payload);
        let stsc = atom(b"stsc", &stsc_payload);
        let stsz = atom(b"stsz", &stsz_payload);
        let mut stbl = [stsd, stsc, stsz].concat();
        let mdat_start = 12;
        let first_chunk = mdat_start + 8;
        stco_payload.extend((first_chunk as u32).to_be_bytes());
        stco_payload.extend(((first_chunk + 3) as u32).to_be_bytes());
        stbl.extend(atom(b"stco", &stco_payload));
        let stbl = atom(b"stbl", &stbl);
        let mdia = atom(b"mdia", &atom(b"minf", &stbl));
        let mut tkhd_payload = vec![0; 16];
        tkhd_payload[12..16].copy_from_slice(&7u32.to_be_bytes());
        let trak = atom(b"trak", &[atom(b"tkhd", &tkhd_payload), mdia].concat());
        let source_moov = atom(b"moov", &trak);
        [
            atom(b"ftyp", b"isom"),
            atom(b"mdat", b"abcdefg"),
            source_moov,
        ]
        .concat()
    }

    #[test]
    fn splices_aimd_chunks_and_rewrites_offsets() {
        let source = synthetic_aimd_mp4();
        let output = [
            atom(b"ftyp", b"isom"),
            atom(b"mdat", b"video"),
            atom(b"moov", b""),
        ]
        .concat();
        let directory = std::env::temp_dir();
        let stem = format!("ffmpeg-aimd-test-{}", std::process::id());
        let source_path = directory.join(format!("{stem}-source.mp4"));
        let input_path = directory.join(format!("{stem}-input.mp4"));
        let output_path = directory.join(format!("{stem}-output.mp4"));
        fs::write(&source_path, source).unwrap();
        fs::write(&input_path, output).unwrap();

        splice_aimd_track(&source_path, &input_path, &output_path).unwrap();
        let result = fs::read(&output_path).unwrap();
        let top = parse_atoms(&result, 0, result.len()).unwrap();
        let moov = top
            .iter()
            .find(|atom| atom.kind == *b"moov")
            .copied()
            .unwrap();
        let aimd = find_aimd_track(&result, moov).unwrap();
        assert_eq!(aimd.chunks.len(), 2);
        assert_eq!(&result[aimd.chunks[0].0..aimd.chunks[0].0 + 3], b"abc");
        assert_eq!(&result[aimd.chunks[1].0..aimd.chunks[1].0 + 4], b"defg");

        let _ = fs::remove_file(source_path);
        let _ = fs::remove_file(input_path);
        let _ = fs::remove_file(output_path);
    }
}
