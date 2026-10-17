"""Opt-in project reads through Hudson's customer HTTP tool boundary (POSIX)."""
import argparse
import hmac
import json
import os
import stat
from http.server import BaseHTTPRequestHandler, HTTPServer

MAX_BYTES = 64 * 1024
MAX_ENTRIES = 200
MAX_SCAN = 1000
RESERVED = {'.git', '.ssh', '.aws', '.azure', '.config', '.codex', '.gnupg',
            '.netrc', '.npmrc', '.pypirc', '.docker', '.kube', '.boto',
            '.git-credentials', 'credentials', 'credentials.json',
            'secrets', 'secrets.json', 'id_rsa', 'id_ed25519', 'id_ecdsa', 'id_dsa'}


def forbidden(name):
    lower = name.lower()
    return (lower in RESERVED or lower.startswith('.env')
            or lower.endswith(('.pem', '.key', '.p12', '.pfx')))


class ProjectFiles:
    def __init__(self, root):
        # Walk from / rather than resolving: even root's parents cannot be symlinks.
        if not isinstance(root, str) or not root.startswith('/'):
            raise ValueError('root must be an absolute directory without symlinks')
        parts = root.split('/')[1:]
        if any(p in ('.', '..') for p in parts):
            raise ValueError('root must be an absolute directory without symlinks')
        fd = os.open('/', os.O_RDONLY | os.O_DIRECTORY)
        try:
            for part in filter(None, parts):
                child = os.open(part, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW,
                                dir_fd=fd)
                os.close(fd)
                fd = child
            self.fd = fd
        except BaseException:
            os.close(fd)
            raise

    def close(self):
        os.close(self.fd)

    def parts(self, path):
        if not isinstance(path, str) or len(path) > 1024 or '\x00' in path:
            raise ValueError('invalid project path')
        if path == '.':
            return []
        parts = path.split('/')
        if any(not p or p in ('.', '..') or forbidden(p) for p in parts):
            raise ValueError('invalid project path')
        return parts

    def open(self, parts, directory):
        fd = os.dup(self.fd)
        try:
            for index, part in enumerate(parts):
                flags = os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK
                if directory or index < len(parts) - 1:
                    flags |= os.O_DIRECTORY
                child = os.open(part, flags, dir_fd=fd)
                os.close(fd)
                fd = child
            return fd
        except BaseException:
            os.close(fd)
            raise

    def read(self, path):
        parts = self.parts(path)
        if not parts:
            raise ValueError('file required')
        fd = self.open(parts, False)
        try:
            info = os.fstat(fd)
            if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1:
                raise ValueError('regular file with one link required')
            if info.st_size > MAX_BYTES:
                raise ValueError('file exceeds read limit')
            with os.fdopen(fd, 'rb', closefd=False) as file:
                data = file.read(MAX_BYTES + 1)
            if len(data) > MAX_BYTES:
                raise ValueError('file exceeds read limit')
            return {'content': data.decode('utf-8'), 'bytes': len(data)}
        finally:
            os.close(fd)

    def list(self, path):
        fd = self.open(self.parts(path), True)
        try:
            entries = []
            scanned = 0
            with os.scandir(fd) as items:
                for item in items:
                    scanned += 1
                    if scanned > MAX_SCAN:
                        raise ValueError('directory exceeds scan limit')
                    if forbidden(item.name) or item.is_symlink():
                        continue
                    info = item.stat(follow_symlinks=False)
                    if stat.S_ISDIR(info.st_mode):
                        kind = 'directory'
                    elif stat.S_ISREG(info.st_mode) and info.st_nlink == 1:
                        kind = 'file'
                    else:
                        continue
                    entries.append({'name': item.name, 'kind': kind})
            entries.sort(key=lambda item: item['name'])
            return {'entries': entries[:MAX_ENTRIES],
                    'truncated': len(entries) > MAX_ENTRIES}
        finally:
            os.close(fd)


def make_server(project, token, port=9002):
    if not isinstance(token, str) or len(token) < 32 or not token.isascii() or any(
            c.isspace() or ord(c) < 33 or ord(c) > 126 for c in token):
        raise ValueError('token must contain at least 32 printable ASCII characters')

    class Handler(BaseHTTPRequestHandler):
        def do_POST(self):
            self.connection.settimeout(5)
            status, result = 200, None
            supplied = self.headers.get('Authorization', '')
            if not hmac.compare_digest(supplied.encode(), ('Bearer ' + token).encode()):
                status, result = 401, {'error': 'unauthorized'}
            elif self.path not in ('/list', '/read'):
                status, result = 404, {'error': 'unknown tool'}
            else:
                try:
                    size = int(self.headers.get('Content-Length', '0'))
                    if not 0 < size <= 4096 or self.headers.get('Transfer-Encoding'):
                        raise ValueError('invalid body')
                    args = json.loads(self.rfile.read(size))
                    if not isinstance(args, dict) or set(args) != {'path'}:
                        raise ValueError('path required')
                    result = (project.list if self.path == '/list' else project.read)(args['path'])
                except (ValueError, TypeError, OSError, UnicodeError):
                    # Tool-level refusal is a successful read result; generic HTTP
                    # transport treats unsuccessful statuses as uncertain outcomes.
                    result = {'error': 'project request refused'}
            body = json.dumps(result, ensure_ascii=True).encode()
            self.send_response(status)
            self.send_header('Content-Type', 'application/json')
            self.send_header('Content-Length', str(len(body)))
            self.send_header('Connection', 'close')
            self.end_headers()
            self.wfile.write(body)
            self.close_connection = True

        def log_message(self, *_):
            pass

    return HTTPServer(('127.0.0.1', port), Handler)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--root', required=True, help='absolute trusted project directory')
    parser.add_argument('--port', type=int, default=9002)
    args = parser.parse_args()
    project = None
    try:
        token = os.environ.get('HUDSON_PROJECT_TOKEN', '')
        project = ProjectFiles(args.root)
        with make_server(project, token, args.port) as server:
            print('Read-only project tools listening on loopback', flush=True)
            server.serve_forever()
    except (ValueError, OSError):
        parser.exit(2, 'Cannot start project tools: check explicit root, token and port.\n')
    finally:
        if project is not None:
            project.close()


if __name__ == '__main__':
    main()
