"""Persistent, owner-only credential for the local trainer API."""

import os
import secrets
import stat
import tempfile

SECRET_FILENAME = ".auth-trainer-secret"
SECRET_ENV = "DECKY_TRAINER_SECRET"


def load_secret(settings_dir: str) -> str:
    """Create once, then reuse. Invalid/unreadable credentials are a hard error.

    Publish a fully written file with a hard link: concurrent plugin starts
    cannot overwrite the winner or observe a partially written credential.
    """
    os.makedirs(settings_dir, mode=0o700, exist_ok=True)
    path = os.path.join(settings_dir, SECRET_FILENAME)
    try:
        return _read_secret(path)
    except FileNotFoundError:
        pass

    fd, temporary = tempfile.mkstemp(prefix=SECRET_FILENAME + ".", dir=settings_dir)
    try:
        with os.fdopen(fd, "w", encoding="ascii") as handle:
            handle.write(secrets.token_hex(32) + "\n")
            handle.flush()
            os.fsync(handle.fileno())
        try:
            os.link(temporary, path)
        except FileExistsError:
            pass  # Another start published its secret first; use that one.
    finally:
        os.unlink(temporary)
    return _read_secret(path)


def _read_secret(path: str) -> str:
    # O_NONBLOCK keeps a FIFO from hanging startup; O_NOFOLLOW rejects symlinks.
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(fd, "rb") as handle:
        metadata = os.fstat(handle.fileno())
        if not stat.S_ISREG(metadata.st_mode) or metadata.st_uid != os.geteuid():
            raise ValueError("trainer authentication secret must be a file owned by this user")
        os.fchmod(handle.fileno(), 0o600)
        raw = handle.read(66)
    secret = raw.removesuffix(b"\n")
    if len(secret) != 64 or any(c not in b"0123456789abcdef" for c in secret):
        raise ValueError(
            "invalid trainer authentication secret; stop all trainers, remove "
            ".auth-trainer-secret from the plugin settings directory, and restart the plugin"
        )
    return secret.decode("ascii")
