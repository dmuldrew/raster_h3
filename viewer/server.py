#!/usr/bin/env python3
"""
Lightweight Static Server with HTTP Range Requests and CORS support for PMTiles.
"""
import os
import re
import sys
from http.server import HTTPServer, SimpleHTTPRequestHandler

class PMTilesHTTPRequestHandler(SimpleHTTPRequestHandler):
    def end_headers(self):
        self.send_header('Accept-Ranges', 'bytes')
        self.send_header('Access-Control-Allow-Origin', '*')
        self.send_header('Access-Control-Allow-Methods', 'GET, HEAD, OPTIONS')
        self.send_header('Access-Control-Allow-Headers', 'Range, Content-Type')
        super().end_headers()

    def do_OPTIONS(self):
        self.send_response(200)
        self.end_headers()

    def send_head(self):
        range_header = self.headers.get('Range')
        if not range_header:
            return super().send_head()

        path = self.translate_path(self.path)
        if not os.path.isfile(path):
            return super().send_head()

        m = re.match(r'bytes=(\d+)-(\d*)', range_header)
        if not m:
            return super().send_head()

        file_size = os.path.getsize(path)
        start = int(m.group(1))
        end = int(m.group(2)) if m.group(2) else file_size - 1
        end = min(end, file_size - 1)
        length = end - start + 1

        if start >= file_size or start > end:
            self.send_error(416, 'Requested Range Not Satisfiable')
            return None

        try:
            f = open(path, 'rb')
            f.seek(start)
        except OSError:
            self.send_error(404, 'File not found')
            return None

        self.send_response(206)
        self.send_header('Content-Type', self.guess_type(path))
        self.send_header('Content-Range', f'bytes {start}-{end}/{file_size}')
        self.send_header('Content-Length', str(length))
        self.send_header('Last-Modified', self.date_time_string(os.path.getmtime(path)))
        self.end_headers()
        return f

def run(port=8080):
    server = HTTPServer(('0.0.0.0', port), PMTilesHTTPRequestHandler)
    print(f"================================================================")
    print(f"🚀 PMTiles HTTP Server running at: http://localhost:{port}/viewer/kepler_pmtiles.html")
    print(f"   (HTTP Byte-Range & CORS enabled)")
    print(f"================================================================")
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        print("\nStopping server.")
        server.server_close()

if __name__ == '__main__':
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8080
    run(port)
