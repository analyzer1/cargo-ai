"""Hash a disposable test cache using the CI Python runtime's native SHA-256."""

import hashlib
import json
from pathlib import Path
import stat
import sys


def inventory(root):
    result = {}

    def visit(path):
        metadata = path.lstat()
        if stat.S_ISLNK(metadata.st_mode) or getattr(metadata, "st_file_attributes", 0) & 0x400:
            raise ValueError("cache links/reparse points are forbidden")
        if stat.S_ISDIR(metadata.st_mode):
            for child in path.iterdir():
                visit(child)
        elif stat.S_ISREG(metadata.st_mode):
            digest = hashlib.sha256()
            with path.open("rb") as source:
                for chunk in iter(lambda: source.read(1024 * 1024), b""):
                    digest.update(chunk)
            result[str(path.relative_to(root))] = digest.hexdigest()
        else:
            raise ValueError("unsupported cache entry")

    visit(root)
    return result


if __name__ == "__main__":
    try:
        print(json.dumps(inventory(Path(sys.argv[1])), sort_keys=True))
    except (OSError, ValueError, IndexError):
        sys.exit("Disposable cache inventory failed")
