import json
import ctypes
import os
from pathlib import Path
import sys
import time

sys.path.insert(0, '/workspace/tests/e2e')
from PIL import ImageGrab, ImageChops
from harness import tree
from harness.application import Application
from harness.browser import Strata
from harness.display import HeadlessDisplay
from harness.environment import TestEnvironment
from harness.fixtures import FixtureTree
from harness.interaction import Keyboard, Pointer
from harness.xtest import XTestConnection

stage = sys.argv[1]
out = Path('/workspace/target/issue-598/evidence') / stage
out.mkdir(parents=True, exist_ok=True)
display = HeadlessDisplay()
display.start()
os.environ.update(display.environment)
compositor = display._spawn('xcompmgr', [
 '/workspace/target/issue-598/tools/usr/bin/xcompmgr', '-n', '-d', display.display])
connection = XTestConnection(display.display)
x11 = connection._x11
x11.XInternAtom.argtypes = [ctypes.c_void_p, ctypes.c_char_p, ctypes.c_int]
x11.XInternAtom.restype = ctypes.c_ulong
x11.XGetSelectionOwner.argtypes = [ctypes.c_void_p, ctypes.c_ulong]
x11.XGetSelectionOwner.restype = ctypes.c_ulong
atom = x11.XInternAtom(connection._display, b'_NET_WM_CM_S0', 0)
deadline = time.monotonic() + 5
while not x11.XGetSelectionOwner(connection._display, atom):
 assert not compositor.exited() and time.monotonic() < deadline, compositor.tail()
 time.sleep(.01)
tree.connect()
results = {}
try:
 for case in (sys.argv[2:] or ['escape', 'outside', 'copy', 'move', 'noop', 'failed', 'reduced']):
  environment = TestEnvironment()
  environment.write_preferences({'reduce_motion': case == 'reduced'})
  settings = environment.config_home / 'gtk-4.0/settings.ini'
  settings.write_text(settings.read_text().replace('gtk-enable-animations=false','gtk-enable-animations=true'))
  fixture = FixtureTree.create_at(Path('/tmp/strata-598-capture'), {'dest': {}, **{f'{c}.txt': f'{c}: fixture 598\n' for c in 'abcdefgh'}})
  if case == 'failed': fixture.path('dest').chmod(0o555)
  app = Application(display, environment, fixture.root)
  browser = Strata(app, Keyboard(connection), Pointer(connection), fixture, environment, display)
  frames=[]
  try:
   app.start()
   source = browser.select_entry('a.txt')
   bounds = source.screen_bounds()
   crop = (bounds.x, bounds.y, bounds.x + bounds.width, bounds.y + bounds.height)
   win = browser.window.screen_bounds()
   capture = lambda: ImageGrab.grab(
    bbox=(win.x,win.y,win.x+win.width,win.y+win.height), xdisplay=display.display)
   dest = browser.entry('dest').screen_bounds().center
   if case in ['escape','outside','reduced']:
    dest=(win.x+win.width+35,win.y+win.height+25)
   elif case == 'noop':
    pane=browser.pane().screen_bounds(); dest=(pane.x+pane.width//2,pane.y+pane.height-20)
   time.sleep(.5)
   connection.motion(*dest)
   time.sleep(.1)
   baseline=capture()
   baseline.save(out/f'{case}-rest.png')
   browser.pointer.drag_points(browser.pointer.drag_origin(source),dest,release=False)
   if case == 'copy':
    connection.key(0xffe3,True); connection.motion(*dest); time.sleep(.1)
   frames.append(capture())
   start=time.monotonic()
   if case in ['escape','reduced']:
    connection.key(0xff1b,True); connection.key(0xff1b,False)
   connection.button(1,False)
   timestamps=[]
   for i in range(40):
    time.sleep(max(0,start+i/60-time.monotonic()))
    frames.append(capture())
    timestamps.append(time.monotonic()-start)
   connection.key(0xffe3,False)
   for ms in [83,150,217,333]:
    idx=min(range(len(timestamps)),key=lambda i:abs(timestamps[i]-ms/1000))
    frames[idx+1].save(out/f'{case}-{ms}ms.png')
   raw = out / f'{case}-frames'
   raw.mkdir(exist_ok=True)
   for index, frame in enumerate(frames):
    frame.save(raw / f'{index:03d}.png')
   frames[0].save(out/f'{case}.gif',save_all=True,append_images=frames[1:],duration=[150]+[20,10,20]*13+[20],loop=0)
   if case in ['copy','move']:
    browser.wait(lambda:fixture.path('dest/a.txt').exists(),'destination file')
    assert fixture.path('dest/a.txt').read_text()=='a: fixture 598\n'
   else: time.sleep(.5)
   exists=fixture.path('a.txt').exists()
   assert exists == (case != 'move'), (case,fixture.listing())
   if exists: assert fixture.path('a.txt').read_text()=='a: fixture 598\n'
   if case not in ['copy','move']: assert list(fixture.path('dest').iterdir())==[]
   results[case]={'source_exists':exists,'listing':fixture.listing(),'timestamps':timestamps,'row_crop':crop,'window':vars(win)}
   (out/f'{case}-application.log').write_text(app.log())
   print(case, 'files verified',flush=True)
  finally:
   app.stop(); fixture.path('dest').chmod(0o755); fixture.cleanup(); environment.cleanup()
finally:
 connection.close(); display.stop()
 (out/'results.json').write_text(json.dumps(results,indent=2))
