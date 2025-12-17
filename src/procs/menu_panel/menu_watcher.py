import socket
import os

client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
client.connect(os.environ["BAR_MENU_WATCHER_SOCK"])


def on_focus_change(boss, window, data):
    if not data["focused"]:
        client.sendall(b"\0")
