"""Bundle the executable with runtime files from the upstream release ZIP."""

import re
import stat
import sys
from pathlib import Path, PurePosixPath
from zipfile import ZIP_DEFLATED, ZipFile, ZipInfo


def main():
  platform, target, tag = sys.argv[1:]
  if not re.fullmatch(r"v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)", tag):
    raise ValueError("Invalid release tag")
  targets = {
    "windows-amd64": "x86_64-pc-windows-msvc",
    "linux-amd64": "x86_64-unknown-linux-gnu",
  }
  if targets.get(platform) != target:
    raise ValueError("Unsupported platform/target combination")
  windows = platform.startswith("windows-")
  executable = "xray-manager.exe" if windows else "xray-manager"
  required = {"xray.dll", "wintun.dll"} if windows else {"libxray.so"}
  binary = Path("target") / target / "release" / executable
  if not binary.is_file() or binary.stat().st_size == 0:
    raise ValueError(f"Missing executable: {binary}")
  archive = Path("upstream") / f"xray-shared-{platform}.zip"
  destination = Path("dist") / f"xray-manager-{tag}-{platform}.zip"

  with ZipFile(archive) as source:
    files = {}
    for member in source.infolist():
      if member.is_dir():
        continue
      path = PurePosixPath(member.filename)
      if path.is_absolute() or ".." in path.parts or "\\" in member.filename or ":" in member.filename:
        raise ValueError(f"Unsafe upstream path: {member.filename}")
      name = path.as_posix()
      if name in files or name == executable:
        raise ValueError(f"Duplicate or reserved upstream path: {name}")
      files[name] = member
    for name in required:
      if name not in files or files[name].file_size == 0:
        raise ValueError(f"Missing required upstream runtime: {name}")
    destination.parent.mkdir(parents=True, exist_ok=True)
    with ZipFile(destination, "w", compression=ZIP_DEFLATED) as output:
      for name, member in files.items():
        # Keep upstream DLL dependencies, licenses and documentation together.
        output.writestr(member, source.read(member))
      info = ZipInfo(executable)
      info.create_system = 3
      info.external_attr = (stat.S_IFREG | 0o755) << 16
      info.compress_type = ZIP_DEFLATED
      output.writestr(info, binary.read_bytes())
  with ZipFile(destination) as result:
    if result.testzip() is not None:
      raise ValueError("Generated ZIP failed its CRC check")
  print(f"Packaged {destination}")


if __name__ == "__main__":
  main()