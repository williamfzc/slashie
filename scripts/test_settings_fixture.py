#!/usr/bin/env python3
"""Create a browser-test fixture for Settings without changing the real bridge."""
from pathlib import Path
import sys

repo = Path(__file__).resolve().parents[1]
source = (repo / 'src/app/assets/index.html').read_text()
bridge = '''<script>
window.fixture = {
  runtime: {polling:true, can_toggle:true, receive_status:'verified', reply_status:'blocked', last_scan_ms:Date.now(), pending_jobs:0, scanning:false, next_scan_in_secs:6, status_text:null},
  settings: {config_text:JSON.stringify({poll_interval:10,routes:[{prefix:'/doubao',cmd:'python3 scripts/doubao_session.py',timeout:90,allow_mentions:false,session:'chat'}]}),identity:'synthetic',identity_status:'connected',log_available:true,config_unparsable:false},
  calls:[], delay:0, failAction:false, failRefresh:false
};
window.__TAURI__ = {core:{invoke:async function(name,args){
  fixture.calls.push({name,args});
  if (name === 'get_status') { if (fixture.failRefresh) throw Error('test offline'); return fixture.runtime; }
  if (name === 'get_settings') return fixture.settings;
  if (name === 'report_settings') return;
  if (name === 'set_polling') {fixture.runtime.polling=args.on; return args.on;}
  if (name === 'save_settings') {
    if (fixture.failAction) throw Error('test config rejected');
    if (args.expected !== fixture.settings.config_text) throw Error('stale configuration');
    fixture.settings.config_text=args.text;return;
  }
  if (name === 'check_connection') return;
  await new Promise(resolve=>setTimeout(resolve,fixture.delay));
  if (fixture.failAction) throw Error('test editor rejected');
}}};
</script>'''
output = Path(sys.argv[1])
output.parent.mkdir(parents=True, exist_ok=True)
output.write_text(source.replace('<script>', bridge + '<script>', 1))
print(output)
