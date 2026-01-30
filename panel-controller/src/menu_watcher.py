import socket
import os

client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
client.connect(os.environ["BAR_MENU_WATCHER_SOCK"])


def on_focus_change(boss, window, data):
    client.sendall(b"\1" if data["focused"] else b"\0")
