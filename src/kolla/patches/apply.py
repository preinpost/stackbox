"""Apply stackbox's Nova backport to a Nova source tree, matching hunks by their text.

The nova_compute image is not guaranteed to ship `patch`, and its Nova revision can
move with every image rebuild, so hunks are located by their exact old text instead
of by line number: a hunk is applied only where its old block occurs exactly once.
A source that holds neither the old nor the new block stops the run, which is what
keeps a failed backport from letting nova_compute start half patched. Every file is
matched, and its result compiled, before any file is written; each one then lands with
an atomic rename, so a reader sees the old file or the new one and never half of either.

    apply.py [--check] PATCH ROOT

`--check` changes nothing and fails unless every hunk is already applied.
"""

import os
import sys
import tempfile


def parse(lines):
    """Group a unified diff into {path: [(old_block, new_block), ...]}."""
    hunks = {}
    path = None
    index = 0
    while index < len(lines):
        line = lines[index]
        index += 1
        if line.startswith("--- a/"):
            path = line[len("--- a/") :].rstrip("\n")
            hunks.setdefault(path, [])
            continue
        if line.startswith("+++ b/") or line.startswith("diff "):
            continue
        if not line.startswith("@@"):
            continue
        if path is None:
            sys.exit("patch hunk before a file name: %r" % line)
        old, new = [], []
        while index < len(lines) and not lines[index].startswith(
            ("@@", "--- a/", "diff ")
        ):
            body = lines[index]
            index += 1
            tag, text = body[:1], body[1:]
            if tag == " ":
                old.append(text)
                new.append(text)
            elif tag == "-":
                old.append(text)
            elif tag == "+":
                new.append(text)
            elif tag == "\\":
                # "\ No newline at end of file": no source line either way.
                continue
            else:
                sys.exit("%s: unrecognised patch line %r" % (path, body))
        hunks[path].append(("".join(old), "".join(new)))
    return hunks


def changed_core(old, new):
    """The part of a hunk that actually changes, without its shared context lines."""
    old_lines, new_lines = old.splitlines(True), new.splitlines(True)
    lead = 0
    while lead < min(len(old_lines), len(new_lines)):
        if old_lines[lead] != new_lines[lead]:
            break
        lead += 1
    trail = 0
    while trail < min(len(old_lines), len(new_lines)) - lead:
        if old_lines[-1 - trail] != new_lines[-1 - trail]:
            break
        trail += 1
    return (
        "".join(old_lines[lead : len(old_lines) - trail]),
        "".join(new_lines[lead : len(new_lines) - trail]),
    )


def apply_hunk(text, old, new, path, number):
    """Return (text, state) for one hunk, where state is 'applied' or 'already'.

    Fails when the source matches neither side, so a Nova change under the backport
    is reported instead of being skipped.
    """
    if text.count(old) == 1:
        return text.replace(old, new), "applied"
    if text.count(new) == 1:
        return text, "already"
    core_old, core_new = changed_core(old, new)
    core_count = text.count(core_old) if core_old else 0
    if core_count == 1:
        # The hunk's own lines match but a neighbouring context line drifted, which an
        # image built between two Nova bugfixes can do. The changed lines are the contract.
        return text.replace(core_old, core_new), "applied"
    # The fallback is skipped when the changed lines occur no times or more than once: with
    # none there is nothing to recognise, and with several there is no way to tell which copy
    # is the backport's without guessing. The error below reports the hunk either way.
    first = old.splitlines()[0] if old.splitlines() else ""
    sys.exit(
        "cannot patch %s: hunk %d matches neither the old nor the new source "
        "(expected a block starting with %r; its changed lines occur %d times)"
        % (path, number, first, core_count)
    )


def stage(source, text):
    """Write `text` beside `source` and return the temporary file's path.

    Keeping the temporary file beside the source makes the final move atomic.
    """
    directory = os.path.dirname(source) or "."
    descriptor, temporary = tempfile.mkstemp(dir=directory, prefix=".stackbox-patch-")
    try:
        with os.fdopen(descriptor, "w") as handle:
            handle.write(text)
        os.chmod(temporary, os.stat(source).st_mode & 0o7777)
    except BaseException:
        os.unlink(temporary)
        raise
    return temporary


def main(argv):
    check = bool(argv) and argv[0] == "--check"
    args = argv[1:] if check else argv
    if len(args) != 2:
        sys.exit("usage: apply.py [--check] PATCH ROOT")
    patch, root = args

    with open(patch) as handle:
        hunks = parse(handle.readlines())
    if not hunks:
        sys.exit("%s holds no hunks" % patch)

    changed = []
    for path in sorted(hunks):
        source = root + "/" + path
        with open(source) as handle:
            original = handle.read()
        text = original
        for number, (old, new) in enumerate(hunks[path], 1):
            text, state = apply_hunk(text, old, new, path, number)
        if text == original:
            print("%s: backport already applied" % path)
            continue
        # A syntax error would leave nova_compute importing a broken driver, so check the
        # whole result before any file is written.
        compile(text, source, "exec")
        changed.append((path, source, text))

    if check:
        if changed:
            sys.exit("nova is not patched (see docs/gpu.md)")
        print("backport in place")
        return
    # Every file passed the hunk matching and the compile; only now touch the source. Each
    # result is staged beside its target and moved in place, so a reader sees the old file or
    # the new one and never a half-written one; a failure while staging leaves the tree as it
    # was, as the docstring promises.
    staged = []
    try:
        for _, source, text in changed:
            staged.append((stage(source, text), source))
        for temporary, source in staged:
            os.replace(temporary, source)
    finally:
        for temporary, _ in staged:
            if os.path.exists(temporary):
                os.unlink(temporary)
    for path, source, text in changed:
        print("%s: patched" % path)


if __name__ == "__main__":
    main(sys.argv[1:])
