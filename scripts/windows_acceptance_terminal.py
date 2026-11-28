"""Receive literal console text for the owned Windows Terminal evaluation tab."""
import ctypes
from ctypes import wintypes
import msvcrt
from pathlib import Path
import sys


def enable_bracketed_paste():
    kernel = ctypes.WinDLL('kernel32', use_last_error=True)
    kernel.GetStdHandle.argtypes = [wintypes.DWORD]
    kernel.GetStdHandle.restype = wintypes.HANDLE
    kernel.GetConsoleMode.argtypes = [wintypes.HANDLE, ctypes.POINTER(wintypes.DWORD)]
    kernel.SetConsoleMode.argtypes = [wintypes.HANDLE, wintypes.DWORD]
    output = kernel.GetStdHandle(-11)
    mode = wintypes.DWORD()
    if not (kernel.GetConsoleMode(output, ctypes.byref(mode))
            and kernel.SetConsoleMode(output, mode.value | 0x04)):
        raise ctypes.WinError(ctypes.get_last_error())
    print('\x1b[?2004h', end='', flush=True)


def main():
    target = Path(sys.argv[1])
    bracketed = sys.argv[2:] == ['--bracketed']
    if bracketed:
        enable_bracketed_paste()
    with target.open("xb", buffering=0) as receiver:
        print("ASR evaluation receiver", flush=True)
        paste_open = False
        while True:
            character = msvcrt.getwch()
            if character in ("\0", "\xe0"):
                msvcrt.getwch()  # Consume the scan code, not receiver text.
            elif bracketed and character == '\x1b':
                suffix = ''.join(msvcrt.getwch() for _ in range(5))
                if suffix != ('[201~' if paste_open else '[200~'):
                    raise RuntimeError('unexpected bracketed-paste marker')
                paste_open = not paste_open
            elif character >= " " or character in ("\n", "\r", "\t"):
                receiver.write(character.encode("utf-16-le", errors="surrogatepass"))


if __name__ == "__main__":
    main()
