#!/usr/bin/env python3
"""Validate a .docx with a real XML parser.

LibreOffice on the dev box has no MS Word 2007 XML filter, so it cannot open a
.docx at all - which means it cannot be used to check ours either. This uses only
the standard library: zipfile for the package and ElementTree for well-formedness.
It will not prove Word accepts the file, but it does catch the failure mode that
matters most, which is a part that is not valid XML.
"""
import sys
import xml.etree.ElementTree as ET
import zipfile

REQUIRED = [
    "[Content_Types].xml",
    "_rels/.rels",
    "word/_rels/document.xml.rels",
    "word/styles.xml",
    "word/document.xml",
]

W = "{http://schemas.openxmlformats.org/wordprocessingml/2006/main}"


def main(path: str) -> int:
    fails = 0

    try:
        z = zipfile.ZipFile(path)
    except Exception as e:  # noqa: BLE001
        print(f"FAIL: not a readable zip: {e}")
        return 1

    bad = z.testzip()
    if bad is not None:
        print(f"FAIL: corrupt entry {bad}")
        fails += 1

    names = set(z.namelist())
    for need in REQUIRED:
        if need not in names:
            print(f"FAIL: missing part {need}")
            fails += 1

    # OOXML requires the content-types part; order is not mandated by the spec
    # but Word is known to be strict about it being first.
    if z.namelist() and z.namelist()[0] != "[Content_Types].xml":
        print(f"WARN: [Content_Types].xml is not the first entry ({z.namelist()[0]})")

    for name in sorted(names):
        if name.endswith("/"):
            continue
        data = z.read(name)
        if name.endswith(".xml") or name.endswith(".rels"):
            try:
                ET.fromstring(data)
            except ET.ParseError as e:
                print(f"FAIL: {name} is not well-formed XML: {e}")
                print(f"      {data[:200]!r}")
                fails += 1

    if "[Content_Types].xml" in names:
        ct = z.read("[Content_Types].xml").decode()
        if "wordprocessingml.document.main+xml" not in ct:
            print("FAIL: content types missing the main document override")
            fails += 1

    if "word/document.xml" in names:
        root = ET.fromstring(z.read("word/document.xml"))
        if root.tag != W + "document":
            print(f"FAIL: root is {root.tag}, expected w:document")
            fails += 1
        body = root.find(W + "body")
        if body is None:
            print("FAIL: no w:body")
            fails += 1
        else:
            paras = body.findall(W + "p")
            texts = [t.text or "" for t in body.iter(W + "t")]
            print(f"ok: {len(paras)} paragraphs, {len(texts)} text runs")
            joined = "".join(texts)
            for probe in ("Tom & Jerry", "alert(1)", "fn a() {}"):
                if probe in joined:
                    print(f"ok: text preserved -> {probe!r}")
            # A raw tag surviving as markup would mean the escape was skipped.
            if "<" in joined and "<script" in joined:
                print("ok: raw html kept as literal text (escaped, not interpreted)")

    if fails:
        print(f"\n{fails} problem(s) in {path}")
        return 1
    print(f"\ndocx package ok: {path}")
    return 0


if __name__ == "__main__":
    if len(sys.argv) != 2:
        print("usage: check-docx.py FILE.docx")
        sys.exit(2)
    sys.exit(main(sys.argv[1]))
