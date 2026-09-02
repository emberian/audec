#!/usr/bin/env python3
"""Send newline-delimited JSON requests to a running audec control socket.
usage: ctl.py '<json>' ['<json>' ...]  -- sends each request line to $AUDEC_CONTROL_SOCKET and prints replies"""
import os, socket, sys, json
path=os.environ.get('AUDEC_CONTROL_SOCKET', os.path.join(os.environ.get('TMPDIR','/tmp'), 'audec-control.sock'))
s=socket.socket(socket.AF_UNIX, socket.SOCK_STREAM); s.settimeout(60); s.connect(path)
f=s.makefile('rw')
for req in sys.argv[1:]:
    f.write(req.strip()+'\n'); f.flush()
    line=f.readline()
    try: print(json.dumps(json.loads(line), indent=None))
    except Exception: print(line.rstrip())
