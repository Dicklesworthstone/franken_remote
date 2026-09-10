#!/usr/bin/env python3
"""Bounded synthetic HEVC -> real Chrome WebCodecs -> canvas experiment.

No compilation, downloads, account profile access, desktop capture or deletion.
Output directories must be new; generated media and browser profile stay there.
"""
import argparse
import base64
import hashlib
import http.server
import json
import os
from pathlib import Path
import signal
import subprocess
import threading


def unhex(dump):
    result = bytearray()
    for line in dump.splitlines():
        if ':' in line:
            data = line.split(':', 1)[1].strip().split('  ', 1)[0]
            result.extend(bytes.fromhex(data))
    return bytes(result)


def codec_string(hvcc):
    if len(hvcc) < 23 or hvcc[0] != 1 or hvcc[21] & 3 != 3:
        raise ValueError('invalid_hvcc_or_nal_width')
    profile = ['', 'A', 'B', 'C'][hvcc[1] >> 6] + str(hvcc[1] & 31)
    compatibility = int(f'{int.from_bytes(hvcc[2:6]):032b}'[::-1], 2)
    constraints = list(hvcc[6:12])
    while constraints and constraints[-1] == 0:
        constraints.pop()
    suffix = ''.join(f'.{value:02X}' for value in constraints)
    return f'hvc1.{profile}.{compatibility:X}.{"H" if hvcc[1] & 32 else "L"}{hvcc[12]}{suffix}'


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--chrome', required=True)
    parser.add_argument('--ffmpeg', default='ffmpeg')
    parser.add_argument('--ffprobe', default='ffprobe')
    parser.add_argument('--headless', action='store_true')
    parser.add_argument('--width', type=int, default=1920)
    parser.add_argument('--height', type=int, default=1080)
    args = parser.parse_args()
    if not (64 <= args.width <= 4096 and 64 <= args.height <= 2160) or args.width % 2 or args.height % 2:
        parser.error('even geometry within 64..4096 by 64..2160 required')
    args.output.mkdir(parents=True, exist_ok=False)
    video = args.output / 'synthetic.mp4'
    with (args.output / 'encode.log').open('w') as log:
        subprocess.run([
            args.ffmpeg, '-nostdin', '-hide_banner', '-f', 'lavfi', '-i',
            f'testsrc2=size={args.width}x{args.height}:rate=30', '-frames:v', '8', '-an',
            '-c:v', 'hevc_videotoolbox', '-allow_sw', '0', '-realtime', '1',
            '-bf', '0', '-g', '60', '-pix_fmt', 'yuv420p', '-tag:v', 'hvc1',
            '-b:v', '8M', '-fs', '16777216', str(video),
        ], stdout=log, stderr=log, check=True, timeout=30)
    if video.stat().st_size > 16 * 1024 * 1024:
        raise ValueError('media_bound')
    packet_file = args.output / 'packets.json'
    with packet_file.open('wb') as packet_output:
        subprocess.run([
            args.ffprobe, '-v', 'error', '-show_streams', '-show_packets', '-show_data',
            '-of', 'json', str(video),
        ], stdout=packet_output, check=True, timeout=15)
    if packet_file.stat().st_size > 64 * 1024 * 1024:
        raise ValueError('probe_output_bound')
    info = json.loads(packet_file.read_bytes())
    stream, = info['streams']
    if stream['codec_name'] != 'hevc' or stream['profile'] != 'Main' or stream['pix_fmt'] != 'yuv420p':
        raise ValueError('unsupported_stream')
    hvcc = unhex(stream['extradata'])
    samples = []
    for packet in info['packets']:
        data = unhex(packet['data'])
        if len(data) != int(packet['size']) or len(data) > 4 * 1024 * 1024:
            raise ValueError('sample_bound')
        cursor, types = 0, []
        while cursor < len(data):
            size = int.from_bytes(data[cursor:cursor + 4])
            cursor += 4
            if size < 2 or cursor + size > len(data):
                raise ValueError('invalid_nal')
            types.append((data[cursor] >> 1) & 63)
            cursor += size
        if any(kind in (32, 33, 34) for kind in types):
            raise ValueError('in_band_parameters_in_hvc1')
        vcl = [kind for kind in types if kind < 32]
        if not vcl or any(kind not in (1, 19, 20) for kind in vcl):
            raise ValueError('unexpected_picture_type')
        key = all(kind in (19, 20) for kind in vcl)
        if key != ('K' in packet['flags']) or packet['pts'] != packet['dts']:
            raise ValueError('key_flag_or_reordering')
        samples.append({'key': key, 'data': base64.b64encode(data).decode(), 'nalTypes': types})
    if len(samples) != 8 or not samples[0]['key'] or any(s['key'] for s in samples[1:]):
        raise ValueError('expected_one_idr_then_seven_pictures')
    fixture = {'width': stream['width'], 'height': stream['height'], 'codec': codec_string(hvcc),
               'description': base64.b64encode(hvcc).decode(), 'samples': samples}
    fixture_bytes = json.dumps(fixture).encode()
    (args.output / 'fixture.json').write_bytes(fixture_bytes)
    script = Path(__file__).with_name('probe.js').read_bytes()
    completed = threading.Event()
    result = []

    class Handler(http.server.BaseHTTPRequestHandler):
        def setup(self):
            super().setup()
            self.connection.settimeout(3)

        def log_message(self, *_):
            pass

        def do_GET(self):
            assets = {'/': (b'<!doctype html><meta charset="utf-8"><script src="/probe.js" defer></script>', 'text/html'),
                      '/probe.js': (script, 'text/javascript'),
                      '/fixture.json': (fixture_bytes, 'application/json')}
            if self.path not in assets:
                self.send_error(404); return
            body, mime = assets[self.path]
            self.send_response(200)
            self.send_header('Content-Type', mime)
            self.send_header('Content-Length', str(len(body)))
            self.send_header('Cache-Control', 'no-store')
            self.send_header('Content-Security-Policy', "default-src 'self'; frame-ancestors 'none'")
            self.end_headers(); self.wfile.write(body)

        def do_POST(self):
            size = int(self.headers.get('Content-Length', '0'))
            if self.path != '/result' or not 0 < size <= 65536 or result:
                self.send_error(400); return
            if self.headers.get('Origin') != origin:
                self.send_error(403); return
            result.append(json.loads(self.rfile.read(size)))
            self.send_response(204); self.end_headers(); completed.set()

    server = http.server.HTTPServer(('127.0.0.1', 0), Handler)
    origin = f'http://127.0.0.1:{server.server_port}'
    threading.Thread(target=server.serve_forever, daemon=True).start()
    command = [args.chrome, '--user-data-dir=' + str(args.output.resolve() / 'profile'),
               '--no-first-run', '--no-default-browser-check', '--disable-background-networking',
               '--remote-debugging-port=0', f'--window-size={args.width},{args.height}']
    if args.headless:
        command.append('--headless=new')
    with (args.output / 'browser.log').open('w') as log:
        browser = subprocess.Popen(command + [origin], stdout=log, stderr=log, start_new_session=True)
        try:
            if not completed.wait(25):
                result.append({'status': 'blocked', 'reason': 'browser_result_timeout'})
        finally:
            if browser.poll() is None:
                os.killpg(browser.pid, signal.SIGTERM)
                try:
                    browser.wait(timeout=3)
                except subprocess.TimeoutExpired:
                    os.killpg(browser.pid, signal.SIGKILL); browser.wait()
            server.shutdown(); server.server_close()
    result[0].update(headless=args.headless, fixtureSha256=hashlib.sha256(fixture_bytes).hexdigest(),
                     probeSha256=hashlib.sha256(script).hexdigest(),
                     runnerSha256=hashlib.sha256(Path(__file__).read_bytes()).hexdigest())
    (args.output / 'result.json').write_text(json.dumps(result[0], indent=2) + '\n')
    print(json.dumps(result[0], indent=2))
    return 0 if result[0]['status'] == 'passed' else 2


if __name__ == '__main__':
    raise SystemExit(main())
