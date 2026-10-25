"""Caption a successful record.py capture and produce the README MP4/GIF.

Requires ffmpeg/ffprobe and the DejaVu Sans fonts. Only initial loading is
trimmed; the recorded Grafana data and navigation remain unchanged.
"""
import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--input', type=Path, default=Path('target/correlation/video-logs'))
    parser.add_argument('--output', type=Path, default=Path('docs/media'))
    args = parser.parse_args()
    source, output = args.input.resolve(), args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    checks = json.loads((source / 'browser-check.json').read_text())
    assert checks['passed'], 'Only render a completed, verified browser walkthrough'
    events = {item['name']: item['seconds'] for item in json.loads((source / 'capture-events.json').read_text())}
    raw = source / 'correlation-raw.webm'
    probe = json.loads(subprocess.check_output([
        'ffprobe', '-v', 'error', '-show_entries', 'stream=width,height,r_frame_rate',
        '-show_entries', 'format=duration', '-of', 'json', str(raw)], text=True))
    width, height = (probe['streams'][0][key] for key in ('width', 'height'))
    assert (width, height) == (1440, 900), (width, height)
    duration = float(probe['format']['duration'])
    numerator, denominator = map(int, probe['streams'][0]['r_frame_rate'].split('/'))
    fps = numerator / denominator
    # Screencast frame time can drift from the CLI's wall clock. Align each
    # caption with its captured UI screenshot, using ffmpeg's SSIM comparison.
    aligned, alignment = {}, {}
    previous = 0
    for name, wall_seconds in events.items():
        if wall_seconds is None or name == 'end':
            continue
        stats = source / (name + '-ssim.txt')
        subprocess.run(['ffmpeg', '-hide_banner', '-loglevel', 'error', '-y', '-i', str(raw),
                        '-loop', '1', '-framerate', str(fps), '-i', str(source / (name + '.png')),
                        '-filter_complex', '[0:v]scale=320:200,format=gray[v];[1:v]scale=320:200,format=gray[r];'
                        f'[v][r]ssim=stats_file={stats.name}:shortest=1',
                        '-filter_complex_threads', '1', '-an', '-f', 'null', '-'], cwd=source, check=True)
        center = wall_seconds * duration / events['end']
        candidates = [((int(frame)-1)/fps, float(score)) for frame, score in
                      re.findall(r'n:(\d+).*All:([\d.]+)', stats.read_text())
                      if max(previous, center-4) <= (int(frame)-1)/fps <= min(duration, center+4)]
        best = max(score for _, score in candidates)
        assert best > 0.95, (name, 'recorded frame differs from screenshot', best)
        timestamp, score = next(pair for pair in candidates if pair[1] >= best - 0.0005)
        aligned[name] = previous = timestamp
        alignment[name] = dict(wall_seconds=wall_seconds, video_seconds=timestamp, ssim=score)
    events = aligned
    trim = max(0, events['01-metrics'] - 0.15)
    end = duration
    font = Path('/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf')
    bold = font.with_name('DejaVuSans-Bold.ttf')
    assert font.is_file() and bold.is_file()
    filters = [f'trim=start={trim}:end={end}', 'setpts=PTS-STARTPTS', 'fps=24',
               'pad=iw:ih+96:0:96:color=0x10131c']

    def text(name, value, x, y, size, color, first=None, last=None, strong=False):
        filename = name + '.txt'
        (source / filename).write_text(value)
        value = f'drawtext=fontfile={bold if strong else font}:textfile={filename}:x={x}:y={y}:fontsize={size}:fontcolor={color}'
        if first is not None:
            start = max(0, events[first] - trim)
            stop = end - trim if last is None else events[last] - trim
            value += f":enable='gte(t,{start})*lt(t,{stop})'"
        filters.append(value)

    text('brand', 'MARS', 32, 18, 25, '0xf4f5f8', strong=True)
    text('brand-sub', 'GSTREAMER', 32, 54, 14, '0x9ea8b8')
    chapters = [
        ('01-metrics', '03-trace-split', '01  METRICS', '0x9bd68e'),
        ('03-trace-split', '05-log-split', '02  TRACES', '0xffb875'),
        ('05-log-split', '07-return-trace', '03  LOGS', '0x87c8ff'),
        ('07-return-trace', '09-profile-query', '04  SAME TRACE', '0xffb875'),
        ('09-profile-query', None, '05  PROFILES', '0xc4a6f4'),
    ]
    for index, (first, last, title, color) in enumerate(chapters):
        text(f'chapter{index}', title, 210, 17, 25, color, first, last, True)
    captions = [
        ('01-metrics', '02-exemplar', 'Find a slow frame, then inspect its recorded exemplar.'),
        ('02-exemplar', '03-trace-split', 'This 500 ms measurement carries the trace and span IDs.'),
        ('03-trace-split', '05-log-split', 'Select the slow identity span, then open its matching log.'),
        ('05-log-split', '06-matching-log', 'The span opens exactly one warning from this frame.'),
        ('06-matching-log', '07-return-trace', 'Native trace/span IDs link the warning back to the same trace.'),
        ('07-return-trace', '09-profile-query', "Back at the same trace. Follow the span's CPU profile."),
        ('09-profile-query', '10-profile', 'The profile is filtered to this service and span.'),
        ('10-profile', None, '500 ms of CPU under [element] heavy, down to the hot function.'),
    ]
    for index, (first, last, caption) in enumerate(captions):
        text(f'caption{index}', caption, 210, 57, 20, '0xc5cbd5', first, last)
    filters.append('drawbox=x=0:y=93:w=iw:h=3:color=0x343b4f:t=fill')
    mp4, gif = output / 'correlation.mp4', output / 'correlation.gif'
    subprocess.run(['ffmpeg', '-hide_banner', '-loglevel', 'warning', '-y', '-i', str(raw),
                    '-vf', ','.join(filters), '-an', '-c:v', 'libx264', '-preset', 'slow',
                    '-crf', '22', '-pix_fmt', 'yuv420p', '-threads', '2', '-movflags', '+faststart',
                    '-metadata', 'title=Mars: metrics, traces, logs and profiles', str(mp4)], cwd=source, check=True)
    scale = 'fps=8,scale=960:-1:flags=lanczos'
    palette = source / 'palette.png'
    # Two passes avoid buffering the entire animation while building its palette.
    subprocess.run(['ffmpeg', '-hide_banner', '-loglevel', 'warning', '-y', '-i', str(mp4),
                    '-vf', scale + ',palettegen=max_colors=128:stats_mode=diff',
                    '-frames:v', '1', '-update', '1', str(palette)], check=True)
    subprocess.run(['ffmpeg', '-hide_banner', '-loglevel', 'warning', '-y', '-i', str(mp4), '-i', str(palette),
                    '-filter_complex', f'[0:v]{scale}[v];[v][1:v]paletteuse=dither=bayer:bayer_scale=3:diff_mode=rectangle',
                    '-loop', '0', str(gif)], check=True)
    metadata = dict(raw_seconds=duration, initial_trim_seconds=trim, rendered_seconds=end-trim,
                    width=width, height=height+96, mp4_fps=24, gif_width=960, gif_fps=8,
                    caption_alignment=alignment,
                    sha256={p.name: hashlib.sha256(p.read_bytes()).hexdigest() for p in (raw, mp4, gif)})
    (source / 'media.json').write_text(json.dumps(metadata, indent=2) + '\n')
    print(json.dumps(metadata, indent=2))


if __name__ == '__main__':
    main()
