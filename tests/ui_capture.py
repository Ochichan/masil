#!/usr/bin/env python3
"""Capture actual PTY output. PNGs reproduce terminal cells, not UI mockups.

Run with .build/bench-venv/bin/python tests/ui_capture.py. Pillow is needed only
for this review artifact exporter; it is not a product dependency.
"""
import hashlib
import json
from pathlib import Path
import time

from PIL import Image, ImageDraw, ImageFont

from test_agentd import AgentdHarness, AGENT
from test_compatibility import RMUX
from test_ui import Terminal

ROOT = Path(__file__).resolve().parents[1]
OUTPUT = ROOT / ".impeccable/review"
COLORS = {"black":"000000", "red":"cd0000", "green":"00cd00", "brown":"cdcd00",
          "blue":"0000ee", "magenta":"cd00cd", "cyan":"00cdcd", "white":"e5e5e5",
          "brightblack":"7f7f7f", "brightred":"ff0000", "brightgreen":"00ff00",
          "brightbrown":"ffff00", "brightblue":"5c5cff", "brightmagenta":"ff00ff",
          "brightcyan":"00ffff", "brightwhite":"ffffff"}


def capture(ui, name):
    ui.drain(.3)
    width, height = ui.screen.columns, ui.screen.lines
    cells = [[dict(ui.screen.buffer[y][x]._asdict()) for x in range(width)] for y in range(height)]
    (OUTPUT / f"{name}.txt").write_text("\n".join(line.rstrip() for line in ui.screen.display).rstrip() + "\n")
    (OUTPUT / f"{name}.json").write_text(json.dumps({"columns":width,"rows":height,"cells":cells}, ensure_ascii=False))
    (OUTPUT / f"{name}.ansi").write_bytes(ui.output)
    font = ImageFont.truetype("/System/Library/Fonts/Menlo.ttc", 18)
    korean = ImageFont.truetype("/System/Library/Fonts/AppleSDGothicNeo.ttc", 18)
    cw, ch = 12, 26
    image = Image.new("RGB", (width*cw, height*ch), "#0c1016")
    draw = ImageDraw.Draw(image)

    def color(value, fallback):
        return "#" + (fallback if value == "default" else COLORS.get(value, value))

    for y, row in enumerate(cells):
        for x, cell in enumerate(row):
            fg, bg = color(cell["fg"], "e6ebf1"), color(cell["bg"], "0c1016")
            if cell["reverse"]:
                fg, bg = bg, fg
            draw.rectangle((x*cw, y*ch, (x+1)*cw-1, (y+1)*ch-1), fill=bg)
    for y, row in enumerate(cells):
        for x, cell in enumerate(row):
            if not cell["data"]:
                continue
            fg = color(cell["bg"] if cell["reverse"] else cell["fg"], "e6ebf1")
            face = korean if any(ord(c) > 0x2fff for c in cell["data"]) else font
            draw.text((x*cw, y*ch+3), cell["data"], font=face, fill=fg,
                      stroke_width=0.3 if cell["bold"] else 0)
    image.save(OUTPUT / f"{name}.png")


def main():
    OUTPUT.mkdir(parents=True, exist_ok=True)
    harness = AgentdHarness()
    harness.setUp()
    terminals = []
    try:
        config = json.loads(harness.config.read_text())
        sessions = []
        for index in range(16):
            sid = f"ses_{index}"
            harness.provider.sessions[sid] = {"id":sid,"directory":harness.provider.directory,"version":"1.18.32"}
            sessions.append({"id":f"agent-{index:02}","pane_id":harness.panes[index%2],"session_id":sid})
            if index%3 == 0:
                harness.provider.statuses[sid] = {"type":"busy"}
        config["sources"][0]["sessions"] = sessions
        harness.config.write_text(json.dumps(config))
        harness.provider.permissions = [{"id":"per_a","sessionID":"ses_0"}, {"id":"per_b","sessionID":"ses_3"}]
        harness.provider.questions = [{"id":"que_a","sessionID":"ses_1"}]
        harness.start()
        end = time.monotonic()+8
        while time.monotonic() < end:
            if harness.query("inspect", id="agent-00")["observation"]["native"]["freshness"] == "fresh":
                break
            time.sleep(.03)
        for name, width, height, args in [
            ("wide",120,32,[]), ("ordinary",80,24,[]), ("sidebar",34,24,["--compact"]),
            ("korean",120,32,["--lang","ko"]), ("light",120,32,["--theme","light"]),
        ]:
            ui = Terminal([AGENT,"--socket",harness.manager,"ui","--core-native",harness.core.socket,*args],
                          harness.core.env | {"XDG_CONFIG_HOME":str(harness.core.path/"config")},width,height)
            terminals.append(ui)
            ui.until("agent-00")
            capture(ui,name)
            if name == "wide":
                ui.send("?")
                capture(ui,"help")
                ui.send("\x1b")
                x,y = ui.locate("agent-00")
                ui.mouse(x,y,code=2)
                capture(ui,"context")
                ui.send("\x1b")
                ui.send("/nothing-matches")
                capture(ui,"empty")
                ui.send("\x15\x1b")
                ui.resize(24,8)
                capture(ui,"tiny")
                ui.resize(120,32)
            if name != "wide":
                ui.send("q")
        harness.query("stop")
        harness.child.communicate(timeout=5)
        terminals[0].until("disconnected")
        capture(terminals[0],"disconnected")
        (OUTPUT/"capture.json").write_text(json.dumps({
            "agent_sha256":hashlib.sha256(AGENT.read_bytes()).hexdigest(),
            "core_sha256":hashlib.sha256(RMUX.read_bytes()).hexdigest(),
            "source":"real rmux-agent PTYs with private core/agentd and deterministic provider fixture",
            "renderer":"pyte terminal cell grid to Pillow; ANSI/text/cell JSON retained",
            "text_export":"Trailing padding omitted; complete geometry retained in cell JSON and ANSI",
            "fonts":["Menlo 18px", "Apple SD Gothic Neo 18px for CJK"],
            "cell_pixels":[12,26],"host":"macOS","native_sidebar_note":"sidebar.png is the compact UI at 34 columns; native creation/zoom tested separately"
        },indent=2))
        print(OUTPUT)
    finally:
        for terminal in terminals:
            terminal.close()
        harness.doCleanups()


if __name__ == "__main__":
    main()
