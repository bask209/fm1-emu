#!/usr/bin/env bash
# The APK in the Android emulator (CI): install it, give it SLOOP, start it, play a key and
# PLAY with touches, and keep screenshots and the log. Fails if it does not boot the firmware.
#   android/ci/emulator-test.sh APK FIRMWARE.fwsc OUTDIR
set -u
APK=$1 FW=$2 OUT=$3
PKG=io.github.bask209.fm1emu
mkdir -p "$OUT"
adb wait-for-device
adb shell getprop ro.build.version.release
adb shell wm size
adb install -r -g "$APK" || exit 1
adb logcat -c

# the firmware where the app looks for it (a debuggable app: run-as reaches its folder)
adb push "$FW" /data/local/tmp/fw.fwsc
adb shell "run-as $PKG mkdir -p files"
adb shell "run-as $PKG cp /data/local/tmp/fw.fwsc files/firmware.fwsc" \
  || adb exec-in "run-as $PKG sh -c 'cat > files/firmware.fwsc'" < "$FW"
adb shell "run-as $PKG sh -c 'echo SLOOP > files/firmware.name; ls -l files'"

# (no "Viewing full screen" hint over the panel)
adb shell settings put secure immersive_mode_confirmations confirmed
adb shell am start -W -n $PKG/.MainActivity
sleep 45
adb exec-out screencap -p > "$OUT/1-booted.png"
adb logcat -d -s fm1:V > "$OUT/logcat-boot.txt"

# where the panel's keys are on the screen: from the app's "window W x H: canvas CW x CH (panel at OX, OY)"
read -r WW WH CW CH OX OY < <(grep -o 'window [0-9]* x [0-9]*: canvas [0-9]* x [0-9]* (panel at [0-9]*, [0-9]*)' "$OUT/logcat-boot.txt" \
  | tail -1 | sed -E 's/[^0-9]+/ /g')
# input takes screen coordinates; the window is the screen less the display cutout, on the left
# in landscape (the camera's edge of the portrait screen)
SW=$(adb shell wm size | grep -o '[0-9]*x[0-9]*' | tail -1 | tr x ' ' | awk '{ print ($1 > $2) ? $1 : $2 }')
DX=$((SW - WW))
echo "screen $SW wide, window ${WW}x${WH} at x $DX, canvas ${CW}x${CH}, panel at $OX,$OY"
at() { # panel x y -> screen x y
  awk -v px="$1" -v py="$2" -v ww="$WW" -v cw="$CW" -v ox="$OX" -v oy="$OY" -v dx="$DX" \
    'BEGIN { s = ww / cw; printf "%d %d", dx + (ox + px) * s, (oy + py) * s }'
}
# C4 (matrix key 21: the fifth white key, low on the key = hard), held 2 s; a screenshot while held
read -r KX KY < <(at 384 770)
adb shell input swipe "$KX" "$KY" "$KX" "$KY" 2000 &
sleep 1
adb exec-out screencap -p > "$OUT/2-key-held.png"
wait
# PLAY (matrix button 12)
read -r PX PY < <(at 1163 340)
adb shell input tap "$PX" "$PY"
sleep 6
adb exec-out screencap -p > "$OUT/3-playing.png"
# an encoder (SELECT) turned by a drag upwards
read -r EX EY < <(at 250 135)
adb shell input swipe "$EX" "$EY" "$EX" "$((EY - 120))" 600
sleep 2
adb exec-out screencap -p > "$OUT/4-after-select.png"
adb shell dumpsys media.aaudio > "$OUT/dumpsys-aaudio.txt" 2>&1 || true
adb shell dumpsys audio > "$OUT/dumpsys-audio.txt" 2>&1 || true
adb logcat -d > "$OUT/logcat-all.txt"
adb logcat -d -s fm1:V > "$OUT/logcat.txt"

echo "---- the app's log"
cat "$OUT/logcat.txt"
ok=0
grep -q 'booting SLOOP' "$OUT/logcat.txt" || { echo "FAIL: no boot"; ok=1; }
grep -q 'touch: key 21 down' "$OUT/logcat.txt" || { echo "FAIL: the touch on C4 did not land"; ok=1; }
grep -q 'touch: key 12 down' "$OUT/logcat.txt" || { echo "FAIL: the touch on PLAY did not land"; ok=1; }
grep -q 'halt:' "$OUT/logcat.txt" && { echo "FAIL: the firmware halted"; ok=1; }
grep -q 'panic' "$OUT/logcat.txt" && { echo "FAIL: a panic"; ok=1; }
grep -q 'audio peak [1-9]' "$OUT/logcat.txt" && echo "sound: the firmware played (audio peak > 0)" || echo "warning: no sound logged"
grep -q 'audio: .* Hz' "$OUT/logcat.txt" && echo "audio stream: $(grep -o 'audio: [0-9]* Hz.*' "$OUT/logcat.txt" | head -1)" || echo "warning: no audio stream"
exit $ok
