#!/usr/bin/env bash
# Frame a README screenshot so it reads as a window or a phone on GitHub,
# which strips CSS: rounded corners, a hairline border and a soft shadow are
# baked into the PNG. Usage:
#   scripts/frame-screenshot.sh window in.png out.png [crop WxH+X+Y]
#   scripts/frame-screenshot.sh phone  in.png out.png
set -euo pipefail

kind=$1 in=$2 out=$3 crop=${4:-}
tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT

src="$tmp/src.png"
if [[ -n $crop ]]; then magick "$in" -crop "$crop" +repage "$src"; else cp "$in" "$src"; fi
read -r w h < <(magick identify -format '%w %h\n' "$src")

round() { # file radius border-color border-width -> rounded, stroked, alpha PNG
  local f=$1 r=$2 bc=$3 bw=$4 fw fh
  read -r fw fh < <(magick identify -format '%w %h\n' "$f")
  magick "$f" \
    \( -size "${fw}x${fh}" xc:none -draw "roundrectangle 0,0,$((fw-1)),$((fh-1)),$r,$r" \) \
    -compose DstIn -composite \
    -fill none -stroke "$bc" -strokewidth "$bw" \
    -draw "roundrectangle $((bw/2)),$((bw/2)),$((fw-1-bw/2)),$((fh-1-bw/2)),$r,$r" \
    "$f"
}

case $kind in
  window)
    r=$(( w / 90 )); bw=$(( w >= 2000 ? 3 : 2 ))
    round "$src" "$r" '#d0d7de' "$bw"
    magick "$src" \( +clone -background '#0b1220' -shadow "28x$((w/60))+0+$((w/90))" \) \
      +swap -background none -layers merge +repage "$out"
    ;;
  phone)
    bezel=$(( w / 34 )); r=$(( w / 9 ))
    round "$src" "$r" '#1f2328' 1
    magick -size "$((w+2*bezel))x$((h+2*bezel))" xc:none \
      -draw "roundrectangle 0,0,$((w+2*bezel-1)),$((h+2*bezel-1)),$((r+bezel)),$((r+bezel))" \
      -fill '#1f2328' -draw "roundrectangle 0,0,$((w+2*bezel-1)),$((h+2*bezel-1)),$((r+bezel)),$((r+bezel))" \
      "$src" -geometry "+${bezel}+${bezel}" -composite "$tmp/phone.png"
    magick "$tmp/phone.png" \( +clone -background '#0b1220' -shadow "30x$((w/40))+0+$((w/60))" \) \
      +swap -background none -layers merge +repage "$out"
    ;;
  *) echo "unknown kind: $kind" >&2; exit 2 ;;
esac
magick identify -format '%f %wx%h\n' "$out"
