#!/bin/bash
# Robustness cases derived from a base PDF with qpdf / head / python.
# Usage: robust_cases.sh <corpus_synth_dir>
set -e
C=$1
BASE=$C/h01_paragraphs_pages/doc.pdf
BEXP=$C/h01_paragraphs_pages/expected.txt
mk() { # id note signal expected_file_or_empty extra_json
  mkdir -p "$C/$1"
  if [ -n "$4" ]; then cp "$4" "$C/$1/expected.txt"; else : > "$C/$1/expected.txt"; fi
  python3 -I -c 'import json,sys; d={"id":sys.argv[1],"group":"robust","generator":"robust_cases.sh (qpdf/head/python)","note":sys.argv[2],"signal":sys.argv[3],"checks":[],"furniture":["Société Exemple - Rapport interne","Réf. RI-2026-04","Page "]}; d.update(json.loads(sys.argv[4] or "{}")); json.dump(d,open(sys.argv[5],"w"),ensure_ascii=False,indent=1)' "$1" "$2" "$3" "$5" "$C/$1/case.json"
}
# user password (AES-256)
mkdir -p $C/r01_encrypted_user
qpdf --encrypt --user-password=secret --owner-password=owner --bits=256 -- $BASE $C/r01_encrypted_user/doc.pdf
mk r01_encrypted_user "AES-256 encrypted, user password 'secret' required to open." "Without the password: a clear 'encrypted / password required' error (not garbage, not a crash). The harness also runs once with the password." "$BEXP" '{"password":"secret","expect_error":"encrypted"}'
# owner password only
mkdir -p $C/r02_owner_only
qpdf --encrypt --user-password= --owner-password=owner --bits=128 --use-aes=y --extract=n --print=none --modify=none -- $BASE $C/r02_owner_only/doc.pdf
mk r02_owner_only "AES-128 encrypted with an empty user password and an owner password; permissions forbid extraction, printing, modification." "Opens without a password; text must come out (BioMerge compares, it does not republish)." "$BEXP" ''
# RC4 40-bit legacy
mkdir -p $C/r03_rc4_legacy
qpdf --allow-weak-crypto --encrypt --user-password= --owner-password=owner --bits=40 -- $BASE $C/r03_rc4_legacy/doc.pdf
mk r03_rc4_legacy "Legacy RC4 40-bit encryption with an empty user password (old Acrobat 3 style)." "Opens without a password; needs RC4 support." "$BEXP" ''
# object streams + xref stream
mkdir -p $C/r04_object_streams
qpdf --object-streams=generate --compress-streams=y $BASE $C/r04_object_streams/doc.pdf
mk r04_object_streams "Re-written with object streams and a cross-reference stream (PDF 1.5)." "Normal modern structure; must work." "$BEXP" ''
# linearized
mkdir -p $C/r05_linearized
qpdf --linearize $BASE $C/r05_linearized/doc.pdf
mk r05_linearized "Linearized (fast web view)." "Must work." "$BEXP" ''
# broken xref: uncompressed copy, then xref offsets zeroed
mkdir -p $C/r06_broken_xref
qpdf --object-streams=disable --qdf $BASE $C/r06_broken_xref/tmp.pdf
python3 -I -c '
import re,sys
d=open(sys.argv[1],"rb").read()
i=d.rfind(b"\nxref\n")
head,tail=d[:i],d[i:]
tail=re.sub(rb"\d{10} 00000 n", b"0000000999 00000 n", tail)
open(sys.argv[2],"wb").write(head+tail)' $C/r06_broken_xref/tmp.pdf $C/r06_broken_xref/doc.pdf
rm $C/r06_broken_xref/tmp.pdf
mk r06_broken_xref "Classic xref table whose every offset points to byte 999 (wrong)." "A tolerant reader rebuilds the xref by scanning objects; text must come out." "$BEXP" ''
# truncated at 60 %
mkdir -p $C/r07_truncated
S=$(stat -c %s $C/r06_broken_xref/doc.pdf)
qpdf --object-streams=disable --qdf $BASE $C/r07_truncated/tmp.pdf
S=$(stat -c %s $C/r07_truncated/tmp.pdf)
head -c $((S * 60 / 100)) $C/r07_truncated/tmp.pdf > $C/r07_truncated/doc.pdf
rm $C/r07_truncated/tmp.pdf
mk r07_truncated "Uncompressed-structure copy truncated at 60 % of its size (no xref, no trailer, last objects missing)." "Partial text of the surviving pages, or a clean error; never a crash or hang. Expected = full text (recall shows how much survives)." "$BEXP" '{"partial_ok":true}'
# not a PDF / empty file
mkdir -p $C/r08_not_a_pdf $C/r09_zero_bytes
head -c 20000 /dev/urandom > $C/r08_not_a_pdf/doc.pdf
: > $C/r09_zero_bytes/doc.pdf
mk r08_not_a_pdf "20 kB of random bytes named .pdf." "Clean error." "" '{"expect_error":"invalid"}'
mk r09_zero_bytes "Empty file." "Clean error." "" '{"expect_error":"invalid"}'
# garbage inside a content stream (valid structure)
mkdir -p $C/r10_bad_content_stream
qpdf --qdf --object-streams=disable $C/l09_blank_middle_page/doc.pdf $C/r10_bad_content_stream/tmp.pdf
python3 -I -c '
import sys
d=open(sys.argv[1],"rb").read()
# replace the 2nd page content operators with junk of the same length (keeps offsets valid)
i=d.find(b"BT", d.find(b"BT", d.find(b"BT")+2)+2)
junk=b"(unbalanced [<< /Q q 99 Tf ] ET ET ET BT ) Tj"
d=d[:i]+junk+d[i+len(junk):]
open(sys.argv[2],"wb").write(d)' $C/r10_bad_content_stream/tmp.pdf $C/r10_bad_content_stream/doc.pdf; rm $C/r10_bad_content_stream/tmp.pdf
cp $C/l09_blank_middle_page/expected.txt $C/r10_bad_content_stream/expected.txt
mk r10_bad_content_stream "Valid file structure; the first page's content stream has junk operators (unbalanced strings/arrays, stray ET) in the middle of the text." "Keep what can be read, no crash. Expected = original text (recall shows how much survives)." "$C/l09_blank_middle_page/expected.txt" '{"partial_ok":true}'
echo done
