#!/usr/bin/env python3
"""Install account-pool tools and the isolated remote organization launcher."""

from datetime import UTC, datetime
from pathlib import Path
import os
import shutil
import tempfile


def main() -> None:
    source_dir = Path(__file__).resolve().parent
    bin_dir = Path.home() / ".local/bin"
    bin_dir.mkdir(parents=True, exist_ok=True)
    backup = (
        Path.home()
        / ".codex/backups"
        / datetime.now(UTC).strftime("account-tools-%Y%m%dT%H%M%S%fZ")
    )
    backup.mkdir(parents=True, mode=0o700)
    files = {
        "codex-account-next": "codex-account-next.sh",
        "codex-account-status": "codex-account-status.py",
        "codext-org": "codext-org.sh",
    }
    aliases = ("codex-account", "codex-account-pool", "codex-account-next-1")
    for name in (*files, *aliases):
        target = bin_dir / name
        if target.exists() or target.is_symlink():
            shutil.copy2(target, backup / name, follow_symlinks=False)

    with tempfile.TemporaryDirectory(prefix=".codex-account-tools-", dir=bin_dir) as temporary:
        staged = Path(temporary)
        for name, source in files.items():
            path = staged / name
            shutil.copy2(source_dir / source, path)
            path.chmod(0o755)
        for name in aliases:
            (staged / name).symlink_to("codex-account-next")
        for name in (*files, *aliases):
            os.replace(staged / name, bin_dir / name)

    print(f"Installed account tools in {bin_dir}")
    print(f"Account credentials: {Path.home() / '.codex/accounts'}/<profile>/auth.json")
    print("codext-org preserves the remote PM's account unless CODEXT_ORG_ACCOUNT_PROFILE is set")
    print(f"Previous launchers saved in {backup}")


if __name__ == "__main__":
    main()
