"""Receive literal console text for the owned Windows Terminal evaluation tab."""
import msvcrt
from pathlib import Path
import sys


def main():
    target = Path(sys.argv[1])
    with target.open("xb", buffering=0) as receiver:
        print("ASR evaluation receiver", flush=True)
        while True:
            character = msvcrt.getwch()
            if character in ("\0", "\xe0"):
                msvcrt.getwch()  # Consume the scan code, not receiver text.
            elif character >= " " or character in ("\n", "\r", "\t"):
                receiver.write(character.encode("utf-16-le", errors="surrogatepass"))


if __name__ == "__main__":
    main()
