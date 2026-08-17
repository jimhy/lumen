"""极简 SVG <path> 光栅化器：path d 字符串 -> 覆盖率浮点掩码（nonzero winding + 超采样抗锯齿）。

只做一件事：把 24x24 viewBox 的单 path 图标渲染成方形 alpha 掩码。
支持命令：M m L l H h V v C c S s Q q T t A a Z z。
"""

import math
import re

import numpy as np

_TOKEN = re.compile(r"[MmLlHhVvCcSsQqTtAaZz]|[-+]?(?:\d*\.\d+|\d+\.?)(?:[eE][-+]?\d+)?")


def _tokenize(d):
    return _TOKEN.findall(d)


def _flatten_cubic(p0, p1, p2, p3, out, steps):
    for i in range(1, steps + 1):
        t = i / steps
        mt = 1.0 - t
        x = mt * mt * mt * p0[0] + 3 * mt * mt * t * p1[0] + 3 * mt * t * t * p2[0] + t * t * t * p3[0]
        y = mt * mt * mt * p0[1] + 3 * mt * mt * t * p1[1] + 3 * mt * t * t * p2[1] + t * t * t * p3[1]
        out.append((x, y))


def _flatten_quad(p0, p1, p2, out, steps):
    for i in range(1, steps + 1):
        t = i / steps
        mt = 1.0 - t
        x = mt * mt * p0[0] + 2 * mt * t * p1[0] + t * t * p2[0]
        y = mt * mt * p0[1] + 2 * mt * t * p1[1] + t * t * p2[1]
        out.append((x, y))


def _flatten_arc(p0, rx, ry, phi_deg, large_arc, sweep, p1, out, steps):
    """SVG 椭圆弧 -> 折线（W3C 实现说明 F.6.5 的端点参数化 -> 中心参数化）。"""
    x0, y0 = p0
    x1, y1 = p1
    if rx == 0 or ry == 0 or (abs(x0 - x1) < 1e-12 and abs(y0 - y1) < 1e-12):
        out.append((x1, y1))
        return
    rx, ry = abs(rx), abs(ry)
    phi = math.radians(phi_deg)
    cos_p, sin_p = math.cos(phi), math.sin(phi)
    dx2, dy2 = (x0 - x1) / 2.0, (y0 - y1) / 2.0
    x1p = cos_p * dx2 + sin_p * dy2
    y1p = -sin_p * dx2 + cos_p * dy2
    # 半径过小时按 F.6.6 等比放大
    lam = (x1p * x1p) / (rx * rx) + (y1p * y1p) / (ry * ry)
    if lam > 1.0:
        s = math.sqrt(lam)
        rx, ry = rx * s, ry * s
    num = rx * rx * ry * ry - rx * rx * y1p * y1p - ry * ry * x1p * x1p
    den = rx * rx * y1p * y1p + ry * ry * x1p * x1p
    coef = math.sqrt(max(num / den, 0.0))
    if large_arc == sweep:
        coef = -coef
    cxp = coef * rx * y1p / ry
    cyp = -coef * ry * x1p / rx
    cx = cos_p * cxp - sin_p * cyp + (x0 + x1) / 2.0
    cy = sin_p * cxp + cos_p * cyp + (y0 + y1) / 2.0

    def angle(ux, uy, vx, vy):
        dot = ux * vx + uy * vy
        n = math.hypot(ux, uy) * math.hypot(vx, vy)
        if n == 0:
            return 0.0
        a = math.acos(max(-1.0, min(1.0, dot / n)))
        return -a if (ux * vy - uy * vx) < 0 else a

    ux, uy = (x1p - cxp) / rx, (y1p - cyp) / ry
    vx, vy = (-x1p - cxp) / rx, (-y1p - cyp) / ry
    theta0 = angle(1.0, 0.0, ux, uy)
    dtheta = angle(ux, uy, vx, vy)
    if not sweep and dtheta > 0:
        dtheta -= 2 * math.pi
    elif sweep and dtheta < 0:
        dtheta += 2 * math.pi
    n = max(2, int(steps * abs(dtheta) / (2 * math.pi)) + 2)
    for i in range(1, n + 1):
        th = theta0 + dtheta * i / n
        x = cos_p * rx * math.cos(th) - sin_p * ry * math.sin(th) + cx
        y = sin_p * rx * math.cos(th) + cos_p * ry * math.sin(th) + cy
        out.append((x, y))


def parse_path(d, curve_steps=48):
    """解析 path d，返回子路径列表（每个是 [(x, y), ...] 折线，隐式闭合）。"""
    toks = _tokenize(d)
    i = 0
    subpaths = []
    cur = []
    x = y = 0.0
    start_x = start_y = 0.0
    cmd = None
    prev_cmd = None
    last_c2 = None  # 上一条三次曲线的第二控制点（S/s 用）
    last_q1 = None  # 上一条二次曲线的控制点（T/t 用）

    def num():
        nonlocal i
        v = float(toks[i])
        i += 1
        return v

    def flag():
        """arc 的 large-arc/sweep 标志：SVG 允许 '0'/'1' 紧贴后续数字，这里 token 已切开。"""
        return num() != 0.0

    while i < len(toks):
        t = toks[i]
        if t.isalpha():
            cmd = t
            i += 1
        elif cmd in ("M", "m"):
            cmd = "L" if cmd == "M" else "l"  # M 后续坐标对按 L 处理
        if cmd is None:
            i += 1
            continue

        up = cmd.upper()
        rel = cmd.islower()
        if up == "Z":
            if cur:
                subpaths.append(cur)
                cur = []
            x, y = start_x, start_y
            prev_cmd = up
            if i < len(toks) and not toks[i].isalpha():
                i += 1  # Z 不带参数；跟着数字属畸形数据，吞掉以免死循环
            continue
        if up == "M":
            if cur:
                subpaths.append(cur)
            nx, ny = num(), num()
            x, y = (x + nx, y + ny) if rel else (nx, ny)
            start_x, start_y = x, y
            cur = [(x, y)]
        elif up == "L":
            nx, ny = num(), num()
            x, y = (x + nx, y + ny) if rel else (nx, ny)
            cur.append((x, y))
        elif up == "H":
            nx = num()
            x = x + nx if rel else nx
            cur.append((x, y))
        elif up == "V":
            ny = num()
            y = y + ny if rel else ny
            cur.append((x, y))
        elif up == "C":
            a, b, c, dd, e, f = (num() for _ in range(6))
            if rel:
                a, b, c, dd, e, f = x + a, y + b, x + c, y + dd, x + e, y + f
            _flatten_cubic((x, y), (a, b), (c, dd), (e, f), cur, curve_steps)
            last_c2 = (c, dd)
            x, y = e, f
        elif up == "S":
            c, dd, e, f = (num() for _ in range(4))
            if rel:
                c, dd, e, f = x + c, y + dd, x + e, y + f
            if prev_cmd in ("C", "S") and last_c2 is not None:
                a, b = 2 * x - last_c2[0], 2 * y - last_c2[1]
            else:
                a, b = x, y
            _flatten_cubic((x, y), (a, b), (c, dd), (e, f), cur, curve_steps)
            last_c2 = (c, dd)
            x, y = e, f
        elif up == "Q":
            a, b, e, f = (num() for _ in range(4))
            if rel:
                a, b, e, f = x + a, y + b, x + e, y + f
            _flatten_quad((x, y), (a, b), (e, f), cur, curve_steps)
            last_q1 = (a, b)
            x, y = e, f
        elif up == "T":
            e, f = num(), num()
            if rel:
                e, f = x + e, y + f
            if prev_cmd in ("Q", "T") and last_q1 is not None:
                a, b = 2 * x - last_q1[0], 2 * y - last_q1[1]
            else:
                a, b = x, y
            _flatten_quad((x, y), (a, b), (e, f), cur, curve_steps)
            last_q1 = (a, b)
            x, y = e, f
        elif up == "A":
            rx, ry, rot = num(), num(), num()
            large, sweep = flag(), flag()
            e, f = num(), num()
            if rel:
                e, f = x + e, y + f
            _flatten_arc((x, y), rx, ry, rot, large, sweep, (e, f), cur, curve_steps)
            x, y = e, f
        else:
            i += 1
            continue
        prev_cmd = up

    if cur:
        subpaths.append(cur)
    return subpaths


def rasterize(subpaths, size, viewbox=24.0, supersample=4, pad=0.0):
    """nonzero winding 扫描线填充，返回 (size, size) 的 float32 覆盖率 [0,1]。

    `pad` 是 viewBox 单位的四周留白（图形整体缩小并居中）。
    """
    n = size * supersample
    scale = (size - 2 * pad) * supersample / viewbox
    off = pad * supersample

    # 收集所有边：(x0, y0, x1, y1)，坐标已换算到超采样像素空间
    edges = []
    for sp in subpaths:
        if len(sp) < 2:
            continue
        pts = [(px * scale + off, py * scale + off) for px, py in sp]
        if pts[0] != pts[-1]:
            pts.append(pts[0])
        for k in range(len(pts) - 1):
            x0, y0 = pts[k]
            x1, y1 = pts[k + 1]
            if y0 != y1:
                edges.append((x0, y0, x1, y1))
    if not edges:
        return np.zeros((size, size), dtype=np.float32)

    e = np.asarray(edges, dtype=np.float64)
    ex0, ey0, ex1, ey1 = e[:, 0], e[:, 1], e[:, 2], e[:, 3]
    ymin = np.minimum(ey0, ey1)
    ymax = np.maximum(ey0, ey1)
    direction = np.where(ey1 > ey0, 1, -1)
    slope = (ex1 - ex0) / (ey1 - ey0)

    cov = np.zeros((n, n), dtype=np.float32)
    row_lo = max(0, int(np.floor(ymin.min())))
    row_hi = min(n, int(np.ceil(ymax.max())) + 1)
    for row in range(row_lo, row_hi):
        sy = row + 0.5
        hit = (ymin <= sy) & (ymax > sy)
        if not hit.any():
            continue
        xs = ex0[hit] + (sy - ey0[hit]) * slope[hit]
        ds = direction[hit]
        order = np.argsort(xs, kind="stable")
        xs = xs[order]
        ds = ds[order]
        wind = np.cumsum(ds)
        inside = wind != 0
        line = cov[row]
        for k in np.nonzero(inside)[0]:
            if k + 1 >= len(xs):
                break
            a, b = xs[k], xs[k + 1]
            ia, ib = int(math.ceil(a - 0.5)), int(math.ceil(b - 0.5))
            ia = max(ia, 0)
            ib = min(ib, n)
            if ib > ia:
                line[ia:ib] = 1.0
    # 盒式降采样 = 超采样抗锯齿
    return cov.reshape(size, supersample, size, supersample).mean(axis=(1, 3)).astype(np.float32)


def path_mask(d, size, viewbox=24.0, supersample=4, pad=0.0, curve_steps=64):
    return rasterize(parse_path(d, curve_steps), size, viewbox, supersample, pad)
