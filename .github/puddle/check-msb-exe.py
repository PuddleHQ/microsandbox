#!/usr/bin/env python3
"""Check a built msb.exe: embedded .msbver section and imported DLLs.

Usage: check-msb-exe.py <msb.exe> <expected version>

* The .msbver section (what puddle's runtime check reads) must equal the expected
  version byte for byte.
* Every imported DLL must be a Windows system DLL (present in System32) and must not
  be a C runtime DLL: the exe is built with a static CRT, so vcruntime/msvcp/ucrt
  imports mean the build lost +crt-static.
"""

import os
import re
import sys

import pefile

CRT = re.compile(r"^(vcruntime|msvcp|ucrtbase|api-ms-win-crt-|concrt|vccorlib)", re.I)


def main() -> int:
    path, expected = sys.argv[1], sys.argv[2]
    pe = pefile.PE(path, fast_load=False)
    errors = []

    sections = [s for s in pe.sections if s.Name.rstrip(b"\0") == b".msbver"]
    if len(sections) != 1:
        errors.append(f".msbver sections: {len(sections)} (want 1)")
    else:
        s = sections[0]
        raw = s.get_data()[: s.Misc_VirtualSize]
        version = raw.decode("utf-8", "replace")
        print(f".msbver = {version!r}")
        if version != expected:
            errors.append(f".msbver is {version!r}, want {expected!r}")

    system32 = os.path.join(os.environ.get("SystemRoot", r"C:\Windows"), "System32")
    imports = sorted(
        {e.dll.decode().lower() for e in getattr(pe, "DIRECTORY_ENTRY_IMPORT", [])}
        | {e.dll.decode().lower() for e in getattr(pe, "DIRECTORY_ENTRY_DELAY_IMPORT", [])}
    )
    print("imports:", ", ".join(imports))
    for dll in imports:
        if CRT.match(dll):
            errors.append(f"imports C runtime DLL {dll} (static CRT expected)")
        elif not dll.startswith("api-ms-win-") and not os.path.isfile(os.path.join(system32, dll)):
            errors.append(f"imports non-system DLL {dll}")

    for e in errors:
        print(f"::error::{e}")
    return 1 if errors else 0


if __name__ == "__main__":
    sys.exit(main())
