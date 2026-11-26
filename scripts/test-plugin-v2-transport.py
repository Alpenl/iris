#!/usr/bin/env python3
"""Measure ten-minute scoped DirectHttp supply for a fixed 4K sample; no GPU/UI claim."""
import argparse
import hashlib
import importlib.util
import json
import math
import os
from pathlib import Path
import platform
import statistics
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]


def sha(path):
    with path.open('rb') as source:
        return hashlib.file_digest(source, 'sha256').hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--ffmpeg', required=True, type=Path)
    parser.add_argument('--ffprobe', required=True, type=Path)
    parser.add_argument('--release', action='store_true', help='build an optimized test executable')
    parser.add_argument('--output', type=Path, default=ROOT/'target/v2-contracts/transport-receipt.json')
    args = parser.parse_args()
    ffmpeg, ffprobe = args.ffmpeg.resolve(strict=True), args.ffprobe.resolve(strict=True)
    directory = ROOT/'target/v2-contracts/transport-4k'
    directory.mkdir(parents=True, exist_ok=True)
    reference = directory/'reference.mp4'
    command = ['-hide_banner', '-loglevel', 'error', '-nostdin', '-y', '-f', 'lavfi', '-i',
               'testsrc2=size=3840x2160:rate=24:duration=10', '-f', 'lavfi', '-i',
               'sine=frequency=440:sample_rate=48000:duration=10', '-c:v', 'libx264', '-threads', '4',
               '-preset', 'veryfast', '-g', '48', '-sc_threshold', '0', '-pix_fmt', 'yuv420p',
               '-b:v', '12M', '-minrate', '12M', '-maxrate', '12M', '-bufsize', '24M',
               '-x264-params', 'nal-hrd=cbr:force-cfr=1', '-c:a', 'aac', '-b:a', '128k',
               '-movflags', '+faststart', str(reference)]
    subprocess.run([str(ffmpeg), *command], check=True, timeout=300)
    data = json.loads(subprocess.check_output([str(ffprobe), '-v', 'error', '-show_streams',
        '-show_format', '-show_packets', '-of', 'json', str(reference)], timeout=60))
    video = next(s for s in data['streams'] if s['codec_type'] == 'video')
    assert video['width'] == 3840 and video['height'] == 2160
    bins = {}
    first = min(float(p['dts_time']) for p in data['packets'] if 'dts_time' in p)
    for packet in data['packets']:
        bucket = math.floor(float(packet.get('dts_time', first))-first)
        bins[bucket] = bins.get(bucket, 0) + int(packet['size'])
    fixture = {'sha256': sha(reference), 'bytes': reference.stat().st_size, 'width': video['width'],
        'height': video['height'], 'codec': video['codec_name'], 'frame_rate': video['r_frame_rate'],
        'format': data['format'], 'peak_one_second_bytes': max(bins.values()),
        'peak_definition': 'packet bytes, including audio, in fixed one-second DTS bins starting at first DTS; not sliding-window or instantaneous frame peak',
        'packet_bytes_per_second': bins, 'ffmpeg_command': command[:-1]+['reference.mp4'],
        'ffmpeg_sha256': sha(ffmpeg), 'ffprobe_sha256': sha(ffprobe),
        'ffmpeg_version': subprocess.check_output([str(ffmpeg), '-version'], text=True).splitlines()[0]}
    fixture['format'].pop('filename', None)
    (directory/'fixture.json').write_text(json.dumps(fixture, indent=2)+'\n', encoding='utf-8', newline='\n')
    env = dict(os.environ)
    env['IRIS_V2_TRANSPORT_FIXTURE'] = str(directory)
    if os.name == 'nt':
        if 'MPV_LIB_DIR' not in env or not env['MPV_LIB_DIR'].strip():
            env['MPV_LIB_DIR'] = str(ROOT/'target/deps/mpv')
        mpv_dir = Path(env['MPV_LIB_DIR'])
        path_entries = [str(mpv_dir)]
        sibling_bin = mpv_dir.parent / 'bin'
        if mpv_dir.name.lower() == 'lib' and sibling_bin.is_dir():
            path_entries.insert(0, str(sibling_bin))
        env['PATH'] = os.pathsep.join([*path_entries, env.get('PATH', '')])
    build = subprocess.run(['cargo', 'test', '--locked', '-p', 'iris-core', '--test', 'player_scoped_plugin', '--no-run',
        '--message-format=json', *(['--release'] if args.release else [])], cwd=ROOT, env=env,
        stdout=subprocess.PIPE, text=True)
    artifacts = [json.loads(line) for line in build.stdout.splitlines() if line.startswith('{')]
    if build.returncode:
        for item in artifacts:
            if item.get('reason') == 'compiler-message':
                print(item['message'].get('rendered', item['message']['message']), flush=True)
        raise SystemExit(build.returncode)
    executable = Path(next(a['executable'] for a in artifacts if a.get('executable') and a.get('target', {}).get('name') == 'player_scoped_plugin'))
    spec = importlib.util.spec_from_file_location('jit_benchmark', ROOT/'scripts/benchmark-plugin-v2.py')
    helper = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(helper)
    result = {'platform': platform.platform(), 'cpu': platform.processor(), 'machine': platform.machine(),
        'logical_cpus': os.cpu_count(), 'rustc': subprocess.check_output(['rustc', '--version'], text=True).strip(),
        'source_commit': subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=ROOT, text=True).strip(),
        'source_dirty': bool(subprocess.check_output(['git', 'status', '--porcelain'], cwd=ROOT)),
        'test_executable_sha256': sha(executable), 'fixture': fixture,
        'sampling': '20ms; Windows OS peak working set and sampled process CPU (last interval may be absent); no allocation count',
        'cache': 'RAM origin and warmed filesystem; complete sample is repeatedly transferred; no WAN latency injection'}
    rss, cpu = 0, None
    print('Starting ten-minute transport run', flush=True)
    with tempfile.TemporaryFile() as output:
        started = time.monotonic()
        process = subprocess.Popen([str(executable), 'sustained_4k_scoped_transport', '--ignored', '--nocapture', '--test-threads=1'],
            cwd=ROOT, env=env, stdout=output, stderr=subprocess.STDOUT)
        try:
            while process.poll() is None:
                if time.monotonic()-started > 690:
                    raise TimeoutError('transport run exceeded 690 seconds')
                if os.name == 'nt':
                    sample, cpu_sample = helper.windows_sample(process)
                    rss = max(rss, sample or 0)
                    cpu = cpu_sample if cpu_sample is not None else cpu
                else:
                    sample = subprocess.run(['ps', '-o', 'rss=', '-p', str(process.pid)], capture_output=True, text=True).stdout.strip()
                    if sample.isdigit():
                        rss = max(rss, int(sample)*1024)
                time.sleep(.02)
        finally:
            if process.poll() is None:
                process.kill()
            process.wait()
        output.seek(0)
        log = output.read().decode('utf-8', errors='replace')
    print(log, flush=True)
    receipt = next((json.loads(line.split('IRIS_V2_TRANSPORT_RECEIPT=', 1)[1]) for line in log.splitlines() if 'IRIS_V2_TRANSPORT_RECEIPT=' in line), None)
    result.update(exit_code=process.returncode, peak_rss_bytes=rss, cpu_seconds_sampled=cpu, output=log, transport=receipt)
    if receipt:
        speeds = sorted(row['bytes_per_second'] for row in receipt['samples'])
        result['fetch_bytes_per_second'] = {'p05_nearest_rank': speeds[max(0, math.ceil(len(speeds)*.05)-1)],
                                          'p50': statistics.median(speeds), 'minimum': speeds[0]}
        result['regression_budget'] = {'minimum_supply_bytes_per_second': receipt['target_bytes_per_second'],
            'max_transport_deadline_misses': 0, 'max_peak_rss_bytes': math.ceil(rss*1.25),
            'scope': 'single ten-minute pilot, same machine/toolchain/profile/fixture; no decoder/UI acceptance'}
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(result, indent=2)+'\n', encoding='utf-8', newline='\n')
    raise SystemExit(process.returncode if process.returncode else 0 if receipt else 1)


if __name__ == '__main__':
    main()
