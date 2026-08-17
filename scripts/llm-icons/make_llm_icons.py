"""生成 Lumen 内置的 LLM CLI 会话图标（圆角方块底 + 品牌符号），输出 PNG(RGBA)。

用法: python make_llm_icons.py <输出目录> [尺寸]
"""

import os
import re
import sys

import numpy as np
from PIL import Image

from svg_raster import parse_path, rasterize

HERE = os.path.dirname(os.path.abspath(__file__))
SVG_DIR = os.path.join(HERE, "svg")


def load_path(name):
    with open(os.path.join(SVG_DIR, f"{name}.svg"), "r", encoding="utf-8") as f:
        svg = f.read()
    m = re.search(r'\sd="([^"]+)"', svg)
    if not m:
        raise SystemExit(f"{name}.svg 里没找到 path d")
    return m.group(1)


def fit(subpaths, size, inset):
    """把 path 的外接框等比缩放进 [inset, size-inset] 并居中。"""
    xs = [p[0] for sp in subpaths for p in sp]
    ys = [p[1] for sp in subpaths for p in sp]
    x0, x1, y0, y1 = min(xs), max(xs), min(ys), max(ys)
    w, h = x1 - x0, y1 - y0
    avail = size - 2 * inset
    s = avail / max(w, h)
    ox = inset + (avail - w * s) / 2.0 - x0 * s
    oy = inset + (avail - h * s) / 2.0 - y0 * s
    return [[(px * s + ox, py * s + oy) for px, py in sp] for sp in subpaths]


def glyph_mask(name, size, inset, ss=4):
    sp = fit(parse_path(load_path(name), curve_steps=96), size, inset)
    return rasterize(sp, size, viewbox=float(size), supersample=ss)


def rounded_rect_mask(size, radius, inset=0.0, ss=4):
    """圆角方块覆盖率掩码（超采样）；`inset` 为四周内缩的像素数。"""
    n = size * ss
    r = max(radius - inset, 0.0) * ss
    lo = inset * ss
    hi = n - inset * ss
    yy, xx = np.mgrid[0:n, 0:n].astype(np.float64)
    cx, cy = xx + 0.5, yy + 0.5
    dx = np.maximum(np.maximum(lo + r - cx, cx - (hi - r)), 0.0)
    dy = np.maximum(np.maximum(lo + r - cy, cy - (hi - r)), 0.0)
    inside = ((np.hypot(dx, dy) <= r) & (cx >= lo) & (cx <= hi) & (cy >= lo) & (cy <= hi)).astype(
        np.float32
    )
    return inside.reshape(size, ss, size, ss).mean(axis=(1, 3)).astype(np.float32)


def hex_rgb(s):
    s = s.lstrip("#")
    return np.array([int(s[i : i + 2], 16) for i in (0, 2, 4)], dtype=np.float64)


def linear_gradient(size, stops, angle_deg=45.0):
    """线性渐变 RGB 场。stops = [(t, '#rrggbb'), ...]，t 沿 angle 方向 0..1。"""
    yy, xx = np.mgrid[0:size, 0:size].astype(np.float64)
    a = np.radians(angle_deg)
    # 屏幕坐标 y 向下：取 -sin 让 angle=45° 表示「左下 -> 右上」
    u = (xx + 0.5) * np.cos(a) - (yy + 0.5) * np.sin(a)
    u = (u - u.min()) / max(u.max() - u.min(), 1e-9)
    ts = np.array([s[0] for s in stops])
    cols = np.stack([hex_rgb(s[1]) for s in stops])
    out = np.empty((size, size, 3), dtype=np.float64)
    for c in range(3):
        out[:, :, c] = np.interp(u, ts, cols[:, c])
    return out


def compose(size, bg_hex, glyph, fg_hex=None, fg_field=None, radius_ratio=0.225, edge=None):
    """圆角底 + 品牌符号 -> 直通 alpha 的 RGBA uint8 数组。

    `edge=(色, 不透明度)` 画一圈内描边：Lumen 深色主题侧栏是 #232323、浅色是
    #f5f5f5，纯黑底（Codex）与纯白底（Kimi）各自会在其中一种主题里糊掉边界，
    统一加描边比逐主题换图省事得多。
    """
    radius = size * radius_ratio
    base = rounded_rect_mask(size, radius)
    rgb = np.empty((size, size, 3), dtype=np.float64)
    rgb[:] = hex_rgb(bg_hex)
    fg = fg_field if fg_field is not None else np.broadcast_to(hex_rgb(fg_hex), (size, size, 3))
    g = np.clip(glyph, 0.0, 1.0)[:, :, None]
    rgb = rgb * (1.0 - g) + fg * g
    if edge is not None:
        ec, ea = edge
        stroke = max(size / 32.0, 1.0)  # 64px 图上 2px，缩到 20px 显示约 0.6px
        ring = np.clip(base - rounded_rect_mask(size, radius, inset=stroke), 0.0, 1.0)[:, :, None]
        rgb = rgb * (1.0 - ring * ea) + hex_rgb(ec) * ring * ea
    a = np.clip(base, 0.0, 1.0)
    out = np.concatenate([rgb, (a * 255.0)[:, :, None]], axis=2)
    return np.clip(out + 0.5, 0, 255).astype(np.uint8)


# 深底配浅描边、浅底配深描边：两种主题下都留得住边界
LIGHT_EDGE = ("#FFFFFF", 0.16)
DARK_EDGE = ("#000000", 0.14)


def build(size):
    """每个 CLI 一张图：官方品牌符号 + 品牌底色的圆角方块。"""
    return {
        # Anthropic 的 sunburst，底色用官方 coral
        "claude": compose(
            size,
            "#D97757",
            glyph_mask("claude", size, size * 0.17),
            fg_hex="#FFFFFF",
            edge=LIGHT_EDGE,
        ),
        # Codex 是 OpenAI 的 CLI，用 OpenAI 花结 + 黑底白标（与其 app 图标一致）
        "codex": compose(
            size,
            "#0D0D0D",
            glyph_mask("openai", size, size * 0.155),
            fg_hex="#FFFFFF",
            edge=LIGHT_EDGE,
        ),
        # Gemini 的四角星 + 官方蓝紫粉渐变，底色取 Google 深色 surface
        "gemini": compose(
            size,
            "#26282D",
            glyph_mask("googlegemini", size, size * 0.115),
            fg_field=linear_gradient(
                size,
                [(0.0, "#4285F4"), (0.52, "#9B72CB"), (1.0, "#D96570")],
                angle_deg=52.0,
            ),
            edge=LIGHT_EDGE,
        ),
        # Kimi 走白底黑标（与其官网一致），顺带和黑底的 Codex 拉开距离
        "kimi": compose(
            size,
            "#FFFFFF",
            glyph_mask("kimi", size, size * 0.19),
            fg_hex="#0B0B0B",
            edge=DARK_EDGE,
        ),
    }


def main():
    out_dir = sys.argv[1]
    size = int(sys.argv[2]) if len(sys.argv) > 2 else 64
    os.makedirs(out_dir, exist_ok=True)
    for name, arr in build(size).items():
        path = os.path.join(out_dir, f"{name}.png")
        Image.fromarray(arr, "RGBA").save(path, optimize=True)
        print(f"{path}  {size}x{size}  {os.path.getsize(path)}B")


if __name__ == "__main__":
    main()
