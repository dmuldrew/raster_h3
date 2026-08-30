#!/usr/bin/env python3
"""
Lightweight Static HTTP Server with HTTP Range Requests and CORS support for PMTiles.
"""
import os
import re
import sys
from http.server import ThreadingHTTPServer, SimpleHTTPRequestHandler

class PMTilesHTTPRequestHandler(SimpleHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def translate_path(self, path):
        # Strip query string and fragment
        clean_path = path.split('?')[0].split('#')[0]
        # Normalize /pmtiles_viewer/data/* -> /data/*
        if clean_path.startswith('/pmtiles_viewer/data/'):
            clean_path = clean_path[len('/pmtiles_viewer'):]
        return super().translate_path(clean_path)

    def end_headers(self):
        self.send_header('Accept-Ranges', 'bytes')
        self.send_header('Access-Control-Allow-Origin', '*')
        self.send_header('Access-Control-Allow-Methods', 'GET, HEAD, OPTIONS')
        self.send_header('Access-Control-Allow-Headers', 'Range, If-Match, If-None-Match, Content-Type, Authorization, X-Requested-With, *')
        self.send_header('Access-Control-Expose-Headers', 'Content-Range, Content-Length, Accept-Ranges, ETag')
        self.send_header('Access-Control-Max-Age', '86400')
        
        # Never cache HTML pages so updates are immediately visible on reload
        clean_path = self.path.split('?')[0].split('#')[0]
        if clean_path.endswith('.html') or clean_path.endswith('/') or not '.' in clean_path.split('/')[-1]:
            self.send_header('Cache-Control', 'no-cache, no-store, must-revalidate')
            self.send_header('Pragma', 'no-cache')
            self.send_header('Expires', '0')
        else:
            self.send_header('Cache-Control', 'public, max-age=3600')
        super().end_headers()

    def do_OPTIONS(self):
        self.send_response(204)
        self.send_header('Content-Length', '0')
        self.end_headers()

    def guess_type(self, path):
        if path.endswith('.pmtiles'):
            return 'application/vnd.pmtiles'
        return super().guess_type(path)

    def do_GET(self):
        # Redirect root URL to viewer
        if self.path in ('', '/', '/index.html'):
            self.send_response(302)
            self.send_header('Location', '/pmtiles_viewer/')
            self.end_headers()
            return

        f = self.send_head()
        if f:
            try:
                if hasattr(self, '_range_length') and self._range_length is not None:
                    remaining = self._range_length
                    while remaining > 0:
                        chunk_size = min(remaining, 64 * 1024)
                        data = f.read(chunk_size)
                        if not data:
                            break
                        self.wfile.write(data)
                        remaining -= len(data)
                else:
                    self.copyfile(f, self.wfile)
            except (BrokenPipeError, ConnectionResetError):
                pass
            finally:
                f.close()

    def do_HEAD(self):
        if self.path in ('', '/', '/index.html'):
            self.send_response(302)
            self.send_header('Location', '/pmtiles_viewer/')
            self.end_headers()
            return
        f = self.send_head()
        if f:
            f.close()

    def send_head(self):
        self._range_length = None
        path = self.translate_path(self.path)
        
        if not os.path.exists(path) or os.path.isdir(path):
            return super().send_head()

        range_header = self.headers.get('Range')
        if not range_header:
            return super().send_head()

        file_size = os.path.getsize(path)

        # Parse HTTP byte range formats:
        # bytes=100-200, bytes=100-, bytes=-500
        start = None
        end = None

        m = re.match(r'bytes=\s*(\d+)\s*-\s*(\d*)', range_header, re.IGNORECASE)
        if m:
            start = int(m.group(1))
            end = int(m.group(2)) if m.group(2) else file_size - 1
        else:
            suffix_m = re.match(r'bytes=\s*-\s*(\d+)', range_header, re.IGNORECASE)
            if suffix_m:
                suffix_len = int(suffix_m.group(1))
                start = max(0, file_size - suffix_len)
                end = file_size - 1

        if start is None or end is None:
            return super().send_head()

        end = min(end, file_size - 1)
        if start >= file_size or start > end:
            self.send_error(416, 'Requested Range Not Satisfiable')
            return None

        length = end - start + 1

        try:
            f = open(path, 'rb')
            f.seek(start)
            self._range_length = length
        except OSError:
            self.send_error(404, 'File not found')
            return None

        mtime = os.path.getmtime(path)
        etag = f'"{int(mtime)}-{file_size}"'

        self.send_response(206)
        self.send_header('Content-Type', self.guess_type(path) or 'application/octet-stream')
        self.send_header('Content-Range', f'bytes {start}-{end}/{file_size}')
        self.send_header('Content-Length', str(length))
        self.send_header('ETag', etag)
        self.send_header('Last-Modified', self.date_time_string(mtime))
        self.end_headers()
        return f

def run(port=8080):
    # Ensure working directory is always the repository root
    repo_root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    if os.path.isdir(os.path.join(repo_root, 'pmtiles_viewer')):
        os.chdir(repo_root)

    server = ThreadingHTTPServer(('0.0.0.0', port), PMTilesHTTPRequestHandler)
    print("================================================================")
    print(f"🚀 PMTiles Hexagon Studio running at: http://localhost:{port}/pmtiles_viewer/")
    print(f"   Serving directory: {os.getcwd()}")
    print("   (HTTP Byte-Range & CORS enabled)")
    print("================================================================")
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        print("\nStopping server.")
        server.server_close()

if __name__ == '__main__':
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8080
    run(port)
