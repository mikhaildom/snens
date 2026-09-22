#!/usr/bin/env python3
"""Проверки тулов: контракты read/edit/write, включая отказы.

    python3 tools/selftest.py
"""
import json
import os
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))


def tool(name, args, stdin=None, cwd=None):
    p = subprocess.run(
        [os.path.join(HERE, name, name)] + args,
        input=stdin,
        capture_output=True,
        text=True,
        cwd=cwd,
    )
    return p.returncode, p.stdout, p.stderr


def check(cond, what):
    if not cond:
        print(f"FAIL: {what}")
        sys.exit(1)
    print(f"ok: {what}")


def main():
    d = tempfile.mkdtemp(prefix="snens-tools-")
    f = os.path.join(d, "a.txt")
    with open(f, "w") as fh:
        fh.write("alpha\nbeta\ngamma\ndelta\n")

    # --- read ---
    rc, out, _ = tool("read", [f])
    check(rc == 0 and out.splitlines()[1].startswith("     2\tbeta"), "read: cat -n формат")
    rc, out, _ = tool("read", [f, "--offset", "3", "--limit", "1"])
    check(out.splitlines()[0].endswith("gamma") and "показаны строки 3-3 из 4" in out, "read: offset/limit + футер")
    rc, out, _ = tool("read", [f, "--limit", "2"])
    check("показаны строки 1-2 из 4" in out, "read: футер при усечении")
    rc, _, err = tool("read", [os.path.join(d, "нет")])
    check(rc == 1 and "не найден" in err, "read: отсутствующий файл — отказ")
    binf = os.path.join(d, "b.bin")
    open(binf, "wb").write(b"\x00\x01\x02")
    rc, _, err = tool("read", [binf])
    check(rc == 1 and "бинарный" in err, "read: бинарник — отказ")
    rc, out, _ = tool("read", ["--help"])
    check(rc == 0 and out.splitlines()[0].startswith("read —"), "read: --help, строка 1 = суть")

    # --- edit ---
    def edit(args, payload):
        return tool("edit", args, stdin=json.dumps(payload, ensure_ascii=False))

    rc, out, _ = edit([f], {"old_string": "beta", "new_string": "BETA"})
    check(rc == 0 and json.loads(out)["replaced"] == 1, "edit: одна замена")
    check(open(f).read() == "alpha\nBETA\ngamma\ndelta\n", "edit: файл изменён точно")

    rc, _, err = edit([f], {"old_string": "нетакого", "new_string": "x"})
    check(rc == 1 and "не найден" in err, "edit: нет совпадений — отказ")

    rc, _, err = edit([f], {"old_string": "  gamma", "new_string": "x"})
    check(rc == 1 and "похоже на строку 3" in err, "edit: подсказка про отступ")

    with open(f, "w") as fh:
        fh.write("dup\ndup\n")
    rc, _, err = edit([f], {"old_string": "dup", "new_string": "x"})
    check(rc == 1 and "2 раз" in err and "replace_all" in err, "edit: неоднозначность — отказ со строками")
    rc, out, _ = edit([f], {"old_string": "dup", "new_string": "x", "replace_all": True})
    check(json.loads(out)["replaced"] == 2 and open(f).read() == "x\nx\n", "edit: replace_all")

    with open(f, "w") as fh:
        fh.write("one\n")
    before = open(f).read()
    rc, out, _ = edit([f, "--dry-run"], {"old_string": "one", "new_string": "two"})
    check(json.loads(out)["dry_run"] and open(f).read() == before, "edit: --dry-run не пишет")

    os.chmod(f, 0o600)
    edit([f], {"old_string": "one", "new_string": "one2"})
    check(oct(os.stat(f).st_mode & 0o777) == "0o600", "edit: права файла сохраняются")

    rc, _, err = edit([f], {"old_string": "", "new_string": "x"})
    check(rc == 1 and "write" in err, "edit: пустой old_string — отказ с подсказкой")
    rc, _, err = tool("edit", [f], stdin="не json")
    check(rc == 1 and "не JSON" in err, "edit: мусор на stdin — понятная ошибка")

    # --- write ---
    newf = os.path.join(d, "sub", "n.txt")
    rc, out, _ = tool("write", [newf], stdin="привет")
    ev = json.loads(out)
    check(rc == 0 and ev["created"] and open(newf).read() == "привет", "write: создаёт каталоги")
    rc, out, _ = tool("write", [newf], stdin="ещё")
    check(json.loads(out)["overwrote"] and open(newf).read() == "ещё", "write: перезапись отмечена в событии")
    tool("write", [newf, "--append"], stdin="!")
    check(open(newf).read() == "ещё!", "write: --append")

    print("\nвсе проверки тулов прошли")
    return 0


if __name__ == "__main__":
    sys.exit(main())