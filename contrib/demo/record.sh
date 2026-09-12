#!/bin/bash
# record.sh start <out.mp4> | stop  — Spectacle screen capture on Plasma 6.
# Fresh process to start; re-invoke the same CLI to stop (see memory
# screen-recording-moltar). Crop the app window afterward with ffmpeg.
STATE=/tmp/kj-demo-rec.current
case "$1" in
  start) pkill -x spectacle 2>/dev/null; sleep 0.5; echo "$2" > "$STATE"
         nohup spectacle -R s -b -n -o "$2" >/dev/null 2>&1 & echo "recording -> $2" ;;
  stop)  out=$(cat "$STATE"); spectacle -R s -b -n -o "$out"; sleep 4
         ffprobe -v error -show_entries format=duration -of csv=p=0 "$out" 2>/dev/null && echo "stopped: $out" ;;
  *) echo "usage: record.sh start <out.mp4> | stop" >&2; exit 2 ;;
esac
