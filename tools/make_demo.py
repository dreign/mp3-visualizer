"""生成一段测试用立体声鼓点音频 demo_beat.wav（120 BPM，约 16 秒）。

左右声道有可辨识的声像差异，用于验证双盘 L/R 分离显示：
  - 底鼓 Kick：偏左
  - 军鼓 Snare：偏右
  - 踩镲 Hihat：居中偏右
  - 贝斯 Bass：居中
  - 额外的中高频旋律音：只在右声道，让右盘中高频更饱满
"""
import math
import os
import struct
import wave

SR = 44100
BPM = 120.0
BEAT = 60.0 / BPM          # 0.5s
DURATION = 16.0
N = int(SR * DURATION)


def kick(t):
    if t < 0 or t > 0.4:
        return 0.0
    f = 150.0 * math.exp(-t * 22.0) + 45.0
    env = math.exp(-t * 12.0)
    return 0.9 * env * math.sin(2 * math.pi * f * t)


def snare(t):
    if t < 0 or t > 0.25:
        return 0.0
    env = math.exp(-t * 22.0)
    noise = math.sin(t * 12345.678) * math.sin(t * 54321.432)
    tone = math.sin(2 * math.pi * 180.0 * t)
    return 0.5 * env * (0.6 * noise + 0.4 * tone)


def hihat(t):
    if t < 0 or t > 0.09:
        return 0.0
    env = math.exp(-t * 55.0)
    noise = math.sin(t * 76543.21) * math.sin(t * 23456.78)
    return 0.25 * env * noise


def bass(t, freq):
    return 0.18 * math.sin(2 * math.pi * freq * t)


def melody(t, beat_idx):
    """右声道专属的中高频琶音，让右盘整圈频谱更饱满。"""
    notes = [440.0, 554.4, 659.3, 880.0, 659.3, 554.4]
    f = notes[beat_idx % len(notes)]
    env = 0.5 + 0.5 * math.sin(2 * math.pi * (t / BEAT))  # 每拍内起伏
    return 0.12 * env * (math.sin(2 * math.pi * f * t) + 0.5 * math.sin(2 * math.pi * f * 2 * t))


def main():
    out = os.path.join(os.path.dirname(__file__), "demo_beat.wav")
    frames = bytearray()
    bass_notes = [55.0, 55.0, 65.4, 73.4]  # A1 A1 C2 D2
    for i in range(N):
        t = i / SR
        beat_idx = int(t / BEAT)
        in_beat = t - beat_idx * BEAT
        in_eighth = t % (BEAT / 2)

        k = kick(in_beat)
        s = snare(in_beat) if beat_idx % 2 == 1 else 0.0
        h = hihat(in_eighth)
        b = bass(t, bass_notes[(beat_idx // 4) % len(bass_notes)])
        m = melody(t, beat_idx)

        # 左声道：底鼓强、军鼓弱、踩镲弱、贝斯居中、无旋律
        left = k * 1.0 + s * 0.35 + h * 0.4 + b
        # 右声道：底鼓弱、军鼓强、踩镲强、贝斯居中、加旋律
        right = k * 0.45 + s * 1.0 + h * 0.9 + b + m

        left = max(-1.0, min(1.0, left))
        right = max(-1.0, min(1.0, right))
        frames += struct.pack("<hh", int(left * 32767), int(right * 32767))

    with wave.open(out, "wb") as w:
        w.setnchannels(2)
        w.setsampwidth(2)
        w.setframerate(SR)
        w.writeframes(frames)
    print(f"已生成立体声 demo: {out}  ({DURATION:.0f}s, {BPM:.0f} BPM, L=鼓点/R=旋律)")


if __name__ == "__main__":
    main()
