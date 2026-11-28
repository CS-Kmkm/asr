"""Record bytes sent by one local, owned PuTTY evaluation window."""
import socket
import sys
from pathlib import Path


def main():
    port, receiver = sys.argv[1:]
    with socket.socket() as server:
        server.bind(("127.0.0.1", 0))
        server.listen(1)
        server.settimeout(25)
        with Path(port).open("x", encoding="ascii") as output:
            output.write(str(server.getsockname()[1]))
        connection, _ = server.accept()
        with connection, Path(receiver).open("xb", buffering=0) as output:
            connection.settimeout(90)
            connection.sendall(b"ASR evaluation receiver\r\n")
            for chunk in iter(lambda: connection.recv(4096), b""):
                output.write(chunk)
                if output.tell() > 131072:
                    raise ValueError("oversized fixture payload")


if __name__ == "__main__":
    main()
