#!/usr/bin/env python3
"""The snake game drawn in pixels with the kitty graphics protocol — a prop
for screenshots and screen recordings.

Usage:

    python3 tools/snake_hd.py              # play: arrows, WASD or hjkl
    python3 tools/snake_hd.py --auto       # the snake plays itself
    python3 tools/snake_hd.py --length 120 # start longer
    python3 tools/snake_hd.py --size 2     # bigger cells, a closer look

The rules, the starting coil and the autopilot are tools/snake.py's; this
file only draws them, so the two games play the same. Keys are the same
too: space pauses (and draws nothing, so the frame still looks like play),
a toggles the autopilot, r restarts, ? shows the key hints again, q quits.

The snake is not cells but a body: it slides between cells instead of
jumping, its skin slides along with it, it follows its own path with a
slight sway, narrows to a pointed tail, and is lit from the top left with
scales and a blotched back. Its head carries eyes with slit pupils and
flicks a forked tongue now and then, even while it waits for the first
arrow. What it eats goes down its body as a bulge. Everything around the
board stays text in the theme's colors, the board itself is transparent,
and the dots and the border are a neutral grey that reads on a dark and a
light background alike.

The pixels a frame draws do not grow with the snake. Every stretch of body
between the neck and the narrowing tail is one of 12 cell shapes, at one of
2 points of the sway and one of 24 positions of the sliding skin; those
are drawn once into one picture, and a frame only tells the terminal which
part of it each cell shows — a short command a cell. The head and neck, the tail, a swallowed bulge
and the shadow are drawn again each frame, as small pictures. On this
machine pictures go through POSIX shared memory (the terminal reads and
removes each object); where the terminal cannot reach it, as over ssh,
they go zlib-compressed in the stream itself.

Needs numpy, and a terminal that speaks the kitty graphics protocol (bateri,
kitty, Ghostty, WezTerm). Elsewhere it says so and points at tools/snake.py.
"""

import argparse
import base64
import fcntl
import math
import mmap
import os
import random
import select
import signal
import struct
import sys
import termios
import time
import tty
import zlib
from collections import deque

try:
    import numpy as np
except ImportError:
    sys.exit("snake_hd needs numpy (python3 -m pip install numpy); "
             "tools/snake.py plays the same game with the standard library only")

try:
    import _posixshmem  # what multiprocessing.shared_memory is built on
except ImportError:
    _posixshmem = None

from snake import DOWN, LEFT, MIN_H, MIN_W, RIGHT, UP, Game

KEYS = {
    b"\x1b[C": RIGHT, b"\x1bOC": RIGHT, b"d": RIGHT, b"l": RIGHT,
    b"\x1b[D": LEFT, b"\x1bOD": LEFT, b"h": LEFT,
    b"\x1b[A": UP, b"\x1bOA": UP, b"w": UP, b"k": UP,
    b"\x1b[B": DOWN, b"\x1bOB": DOWN, b"s": DOWN, b"j": DOWN,
}

# Image ids. Each is replaced or deleted by id, so a frame never stacks on
# the one before it.
HEAD_ID, TAIL_ID, SHADOW_ID, GROUND_ID, ATLAS_ID = 1, 2, 3, 4, 5
BULGE_ID, BULGES = 10, 4  # ids 10 to 13: a bulge each, at most four at once
FOOD_ID = 20  # 20 to 22: the three fruit
# Layers, from the bottom: all below the text, so the banner and the "too
# small" note are drawn over the pictures. The head lies over the body.
GROUND_Z, FOOD_Z, SHADOW_Z, ATLAS_Z, TAIL_Z, BULGE_Z, HEAD_Z = -30, -20, -15, -12, -11, -10, -9

# The body, as fractions of a cell. The sway plus the widest the body gets
# stays under half a cell, so a stretch of body never leaves its own row or
# column of cells and the next lane over stays clear.
RADIUS = 0.33
BULGE = 0.09  # what a swallowed piece of fruit adds
WIDEST = RADIUS + BULGE
SWAY = 0.065
SWAY_PERIOD = 2  # cells of path per full sway: whole, so cells repeat
NECK = 0.86  # the neck's radius against the body's, just behind the head
NECK_REACH = 1.5  # cells behind the head the neck still differs from the body
TAPER = 3.2  # cells over which the tail narrows to its point
assert SWAY + WIDEST < 0.5

# The skin's pattern repeats every cell (one diamond, six rows of scales),
# so a cell looks the same at the same point of the pattern; it slides in
# this many steps a cell.
PHASES = 24
LEFT_OF = {RIGHT: UP, UP: LEFT, LEFT: DOWN, DOWN: RIGHT}
RIGHT_OF = {turned: heading for heading, turned in LEFT_OF.items()}
# A cell's shape: the way the body comes in, the way it goes out, and which
# half of the sway it is in.
SHAPES = [(come, go, parity) for come in (RIGHT, DOWN, LEFT, UP)
          for go in (come, LEFT_OF[come], RIGHT_OF[come]) for parity in (0, 1)]
SHAPE_ROW = {shape: row for row, shape in enumerate(SHAPES)}

SHADOW_SCALE = 4  # the shadow is drawn at a quarter of the resolution, then stretched

# Light comes from the top left and a little in front of the screen.
LIGHT = np.array([-0.45, -0.62, 0.95], np.float32)
LIGHT /= np.linalg.norm(LIGHT)
HALF = LIGHT + np.array([0, 0, 1], np.float32)
HALF /= np.linalg.norm(HALF)

SKIN = {
    "back": (0.30, 0.52, 0.17),
    "flank": (0.58, 0.66, 0.27),
    "belly": (0.86, 0.84, 0.56),
    "blotch": (0.11, 0.20, 0.07),
    "rim": (0.80, 0.80, 0.46),
    "lip": (0.78, 0.78, 0.47),
    "iris": (0.93, 0.68, 0.12),
    "tongue": (0.80, 0.09, 0.20),
}
SKIN = {name: np.array(rgb, np.float32) for name, rgb in SKIN.items()}

FLICK = 0.5  # seconds a flick of the tongue takes
SWALLOW = 2.4  # seconds a swallowed piece of fruit takes down the body


def smoothstep(edge0, edge1, x):
    t = np.clip((x - edge0) / (edge1 - edge0), 0.0, 1.0)
    return t * t * (3 - 2 * t)


def mix(a, b, t):
    """Blends color `a` toward `b` by `t`, per pixel."""
    return a + (b - a) * t[..., None]


def lit(color, normal, gloss, occlusion):
    """`color` under the light, on a surface facing `normal`: diffuse, a
    specular glint scaled by `gloss`, darkened by `occlusion`."""
    diffuse = np.clip(normal @ LIGHT, 0, 1)
    spec = np.clip(normal @ HALF, 0, 1) ** 28 * 0.42 * gloss
    shaded = color * ((0.40 + 0.75 * diffuse) * occlusion)[..., None]
    return np.clip(shaded + np.asarray(spec)[..., None], 0, 1)


def to_rgba(rgb, alpha):
    """Straight-alpha RGBA bytes from float color and coverage."""
    out = np.empty(alpha.shape + (4,), np.uint8)
    out[..., :3] = np.clip(rgb * 255 + 0.5, 0, 255)
    out[..., 3] = np.clip(alpha * 255 + 0.5, 0, 255)
    return out


def over(top_rgb, top_a, rgb, a):
    """`top` composited over the straight-alpha layer `rgb`, `a`."""
    out_a = top_a + a * (1 - top_a)
    safe = np.where(out_a > 0, out_a, 1)
    out_rgb = (top_rgb * top_a[..., None] + rgb * (a * (1 - top_a))[..., None]) / safe[..., None]
    return out_rgb, out_a


def box_blur(img, radius):
    """A box blur of `radius` pixels along both axes, edges held."""
    if radius < 1:
        return img
    span = 2 * radius + 1
    for axis in (0, 1):
        pad = [(0, 0), (0, 0)]
        pad[axis] = (radius + 1, radius)
        c = np.cumsum(np.pad(img, pad, mode="edge"), axis=axis, dtype=np.float32)
        c = np.moveaxis(c, axis, 0)
        img = np.moveaxis((c[span:] - c[:-span]) / span, 0, axis)
    return img


def shrink(img, factor):
    """`img` averaged down by `factor` along both axes, padded with zeros."""
    h, w = img.shape
    ph, pw = -(-h // factor) * factor, -(-w // factor) * factor
    padded = np.zeros((ph, pw), np.float32)
    padded[:h, :w] = img
    return padded.reshape(ph // factor, factor, pw // factor, factor).mean(axis=(1, 3))


def cell_path(come, go, tau, g):
    """Points of the path through a cell `g` pixels wide, from its top left
    corner, at `tau` (0 at the edge it comes in by, 1 at the edge it leaves
    by): straight, or a quarter circle around the corner the two edges
    share. Also the length of the cell's stretch."""
    come, go = np.array(come, np.float32), np.array(go, np.float32)
    center = np.full(2, g / 2, np.float32)
    tau = np.asarray(tau, np.float32)
    if np.array_equal(come, go):
        return center + np.outer(tau - 0.5, come) * g, g
    corner = center + (go - come) * (g / 2)
    angle = tau * (math.pi / 2)
    return corner + (g / 2) * (np.outer(np.cos(angle), -go) + np.outer(np.sin(angle), come)), g * math.pi / 4


def unit_and_normal(vectors):
    unit = vectors / np.linalg.norm(vectors, axis=1, keepdims=True)
    return unit, np.stack([-unit[:, 1], unit[:, 0]], 1)


def cell_frame(shape, tau, g):
    """Points, unit tangents and unit normals of the swayed path through a
    cell of `shape`, and the length of its stretch. The sway is tied to the
    path, not to time: every part of the body passes the same bends."""
    come, go, parity = shape
    tau = np.asarray(tau, np.float32)

    def swayed(t):
        base, length = cell_path(come, go, t, g)
        ahead, _ = cell_path(come, go, t + 1e-3, g)
        _, normal = unit_and_normal(ahead - base)
        sway = SWAY * g * np.sin(2 * math.pi * (parity - 0.5 + t) / SWAY_PERIOD)
        return base + normal * sway[:, None], length

    pts, length = swayed(tau)
    ahead, _ = swayed(tau + 1e-3)
    tangent, normal = unit_and_normal(ahead - pts)
    return pts, tangent, normal, length


def cell_pixels(shape, g):
    """The pixels of a cell of `shape` the body can reach, as flat arrays:
    row and column from the cell's top left, signed distance from the path
    (along the normal), how far through the cell (0 to 1) the closest point
    of the path is, and the normal there."""
    tau = np.linspace(0, 1, 25, dtype=np.float32)
    pts, tangent, normal, length = cell_frame(shape, tau, g)
    gy, gx = np.mgrid[0:g, 0:g]
    gy, gx = gy.ravel(), gx.ravel()
    pix = np.stack([gx + 0.5, gy + 0.5], 1).astype(np.float32)
    diff = pix[:, None, :] - pts[None, :, :]
    nearest = np.einsum("psk,psk->ps", diff, diff).argmin(1)
    offset = diff[np.arange(len(pix)), nearest]
    side = np.einsum("pk,pk->p", offset, normal[nearest])
    keep = np.abs(side) < WIDEST * g + 1
    nearest, offset, side = nearest[keep], offset[keep], side[keep]
    along = tau[nearest] + np.einsum("pk,pk->p", offset, tangent[nearest]) / length
    return (gy[keep].astype(np.int32), gx[keep].astype(np.int32), side.astype(np.float32),
            along.astype(np.float32), normal[nearest, 0].copy(), normal[nearest, 1].copy())


class Track:
    """The cells the head has entered, numbered in the order it entered them.

    A cell's shape is known once the head has left it, so the picture trails
    the game by a cell and a half and a turn is never drawn before it is
    decided — the head reaches the cell where it turns at the moment the game
    turns it. A position on the path is a number: whole at cell centers."""

    def __init__(self, game, painter, origin):
        self.painter = painter
        self.g = painter.g
        self.origin = origin
        cells = list(reversed(game.snake))  # tail first
        self.cells = dict(enumerate(cells))
        self.head = self.prev_head = len(cells) - 1
        self.tail = self.prev_tail = 0
        self.shapes = {}  # a cell's shape, once the cell after it is known

    def advance(self, game):
        """Follows one step of `game`."""
        self.prev_head, self.prev_tail = self.head, self.tail
        self.head += 1
        self.cells[self.head] = game.snake[0]
        self.tail = self.head - len(game.snake) + 1
        for index in [i for i in self.cells if i < self.tail - 2]:
            del self.cells[index]
            self.shapes.pop(index, None)

    def hold(self):
        self.prev_head, self.prev_tail = self.head, self.tail

    def ends(self, s):
        """Where the head and the tail are drawn, `s` of the way through the
        current step."""
        head = self.prev_head - 0.5 + s * (self.head - self.prev_head)
        tail = self.prev_tail - 0.5 + s * (self.tail - self.prev_tail)
        return head, tail

    def steady(self):
        """The first and last cell that are plain body all through the
        current step: past the narrowing tail and behind the neck."""
        return math.ceil(self.tail + TAPER), math.floor(self.prev_head - 1 - NECK_REACH)

    def shape(self, m):
        if m in self.shapes:
            return self.shapes[m]
        x, y = self.cells[m]
        before, after = self.cells.get(m - 1), self.cells.get(m + 1)
        come = (x - before[0], y - before[1]) if before else None
        go = (after[0] - x, after[1] - y) if after else None
        shape = come or go or RIGHT, go or come or RIGHT, m % 2
        if after:
            self.shapes[m] = shape
        return shape

    def corner(self, m):
        """Board pixel of cell `m`'s top left corner."""
        x, y = self.cells[m]
        return int(self.origin[0] + x * self.g), int(self.origin[1] + y * self.g)

    def pixels(self, m):
        """`cell_pixels` of cell `m`, in board pixels and path positions."""
        ys, xs, side, along, nx, ny = self.painter.cell(self.shape(m))
        x0, y0 = self.corner(m)
        return ys + y0, xs + x0, side, m - 0.5 + along, nx, ny

    def at(self, p):
        """The path's point, tangent and normal at path position `p`."""
        m = max(min(self.cells), min(math.ceil(p - 0.5), self.head - 1))
        tau = min(max(p - (m - 0.5), 0.0), 1.0)
        pt, tangent, normal, _ = cell_frame(self.shape(m), [tau], self.g)
        return pt[0] + self.corner(m), tangent[0], normal[0]

    def pieces(self, p_head, p_tail):
        """The cells the body crosses between its two ends."""
        lo = max(math.floor(p_tail + 0.5), min(self.cells))
        hi = math.ceil(p_head - 0.5)
        return [m for m in range(lo, hi + 1) if m in self.cells and m < self.head]


class Painter:
    """Draws the snake, its shadow, and the pictures that do not move, at
    one cell size: `g` pixels."""

    def __init__(self, g):
        self.g = g
        self._cells = {}
        self._masks = {}
        self._atlas = None

    def cell(self, shape):
        if shape not in self._cells:
            self._cells[shape] = cell_pixels(shape, self.g)
        return self._cells[shape]

    def mask(self, shape):
        """The plain body's coverage of a cell of `shape`, at the shadow's
        resolution."""
        if shape not in self._masks:
            ys, xs, side, *_ = self.cell(shape)
            full = np.zeros((self.g, self.g), np.float32)
            full[ys, xs] = np.clip(RADIUS * self.g - np.abs(side) + 0.5, 0, 1)
            self._masks[shape] = shrink(full, SHADOW_SCALE)
        return self._masks[shape]

    def _shade(self, s, side, r, nx, ny):
        """The lit color and the coverage of body pixels `s` pixels behind
        the head (on the pattern), `side` pixels off the path, where the body
        is `r` pixels thick and the path's normal is (`nx`, `ny`)."""
        w = np.clip(side / r, -1, 1)
        color, normal, gloss, occlusion = self._skin(s, w, r, nx, ny)
        return lit(color, normal, gloss, occlusion), np.clip(r - np.abs(side) + 0.5, 0, 1)

    def _skin(self, s, w, r, nx, ny):
        """The body's color, surface normal, gloss and occlusion at `s`
        pixels behind the head, `w` across it (−1 to 1), where it is `r`
        pixels thick; (`nx`, `ny`) is the path's normal there."""
        g = self.g
        aw = np.abs(w)
        nz = np.sqrt(np.maximum(1 - w * w, 0))
        color = mix(SKIN["back"], SKIN["flank"], smoothstep(0.45, 0.85, aw))
        color = mix(color, SKIN["belly"], smoothstep(0.86, 0.98, aw))
        # A diamond down the back every cell, each with a pale rim, and dark
        # spots on the flanks between them.
        u = s / g
        fu = (u - np.floor(u)) - 0.5
        diamond = np.abs(fu) * 1.9 + aw * 1.15
        color = mix(color, SKIN["rim"], smoothstep(0.98, 0.86, diamond) * 0.85)
        color = mix(color, SKIN["blotch"], smoothstep(0.80, 0.70, diamond))
        fv = (u + 0.5 - np.floor(u + 0.5)) - 0.5
        spot = np.hypot(fv * 2.4, (aw - 0.72) * 3.6)
        color = mix(color, SKIN["blotch"], smoothstep(0.62, 0.48, spot) * 0.9)
        # Scales: a diagonal grid laid on the surface, six rows a cell,
        # darker at the seams, each scale a little brighter at its front.
        across = r * np.arcsin(w)
        k = g / 6
        sa, sb = (s + across) / k, (s - across) / k
        fa, fb = sa - np.floor(sa) - 0.5, sb - np.floor(sb) - 0.5
        seam = smoothstep(0.34, 0.5, np.maximum(np.abs(fa), np.abs(fb)))
        color = color * ((1 - 0.30 * seam) * (1 + 0.10 * (fa + fb)))[..., None]
        # Each scale tilts a little, along the body and across it.
        tilt_along, tilt_across = 0.05 * (fa + fb), 0.05 * (fa - fb)
        normal = np.stack([(w + tilt_across) * nx + tilt_along * ny,
                           (w + tilt_across) * ny - tilt_along * nx, nz], -1)
        normal /= np.linalg.norm(normal, axis=-1, keepdims=True)
        return color, normal, 1 - 0.7 * seam, 0.55 + 0.45 * np.sqrt(nz)

    def atlas(self):
        """Every shape of plain body at every position of the sliding skin:
        a row per shape, a cell per position. Drawn once."""
        if self._atlas is not None:
            return self._atlas
        g = self.g
        out = np.zeros((len(SHAPES) * g, PHASES * g, 4), np.uint8)
        phases = np.arange(PHASES, dtype=np.float32)[:, None] / PHASES
        for row, shape in enumerate(SHAPES):
            ys, xs, side, along, nx, ny = self.cell(shape)
            # Behind the head by the pattern's own measure: a whole number of
            # cells plus the phase, less how far into the cell.
            s = ((phases + 0.5 - along[None, :]) % 1.0) * g
            rgb, alpha = self._shade(s.ravel(), np.tile(side, PHASES), RADIUS * g,
                                     np.tile(nx, PHASES), np.tile(ny, PHASES))
            rows = row * g + np.broadcast_to(ys, s.shape)
            cols = xs[None, :] + (np.arange(PHASES) * g)[:, None]
            out[rows, cols] = to_rgba(rgb, alpha).reshape(PHASES, len(ys), 4)
        self._atlas = out
        return out

    def part(self, track, pieces, ends, phase, bulges, area, head=None, dead=False):
        """Cells `pieces` of the body as they are now — narrowing toward the
        tail, thinner at the neck, fat where something is going down — and,
        with `head` (how far the tongue is out, and the time), the head over
        them. Returns straight-alpha RGBA, its coverage, and the board pixel
        of its top left corner, a multiple of the shadow's scale."""
        g = self.g
        p_head, p_tail = ends
        corners = [track.corner(m) for m in pieces]
        if head is not None:
            head_pt, head_t, head_n = track.at(p_head)
            reach = 1.3 * g
            corners += [(head_pt[0] - reach, head_pt[1] - reach), (head_pt[0] + reach - g, head_pt[1] + reach - g)]
        if not corners:
            return None
        bx0 = max(0, int(min(x for x, _ in corners)) - 2) // SHADOW_SCALE * SHADOW_SCALE
        by0 = max(0, int(min(y for _, y in corners)) - 2) // SHADOW_SCALE * SHADOW_SCALE
        bx1 = min(area[0], int(max(x for x, _ in corners)) + g + 2)
        by1 = min(area[1], int(max(y for _, y in corners)) + g + 2)
        bw, bh = bx1 - bx0, by1 - by0
        if bw <= 0 or bh <= 0:
            return None
        rgb = np.zeros((bh, bw, 3), np.float32)
        alpha = np.zeros((bh, bw), np.float32)
        iy = ix = np.zeros(0, np.int64)

        if pieces:
            iy, ix, side, where, nx, ny = (np.concatenate(part) for part in zip(*(track.pixels(m) for m in pieces)))
            sel = (where >= p_tail) & (where <= p_head + 0.05)
            iy, ix, side, where, nx, ny = iy[sel] - by0, ix[sel] - bx0, side[sel], where[sel], nx[sel], ny[sel]
            length = max(p_head - p_tail, 1e-3)
            from_head = p_head - where
            taper = np.clip((length - from_head) / min(TAPER, 0.45 * length + 0.6), 0, 1) ** 0.62
            neck = 1 - (1 - NECK) * np.exp(-(((from_head - 0.55) / 0.45) ** 2))
            r = RADIUS * g * taper * neck
            for at in bulges:
                r = r + BULGE * g * np.exp(-(((from_head - at) / 0.6) ** 2))
            r = np.maximum(r, 0.01)
            keep = np.abs(side) < r + 0.5
            iy, ix = iy[keep], ix[keep]
            rgb[iy, ix], alpha[iy, ix] = self._shade((phase - where[keep]) * g, side[keep], r[keep],
                                                     nx[keep], ny[keep])

        box_y = box_x = np.zeros(0, np.int64)
        if head is not None:
            # The tongue, under the head it comes out of, then the head.
            tongue, now = head
            hx0, hy0 = max(int(head_pt[0] - reach) - bx0, 0), max(int(head_pt[1] - reach) - by0, 0)
            hx1, hy1 = min(int(head_pt[0] + reach) - bx0, bw), min(int(head_pt[1] + reach) - by0, bh)
            if hx1 > hx0 and hy1 > hy0:
                gy, gx = np.mgrid[hy0:hy1, hx0:hx1].astype(np.float32)
                rel_x = gx + bx0 + 0.5 - head_pt[0]
                rel_y = gy + by0 + 0.5 - head_pt[1]
                a = rel_x * head_t[0] + rel_y * head_t[1]
                b = rel_x * head_n[0] + rel_y * head_n[1]
                box = (slice(hy0, hy1), slice(hx0, hx1))
                if tongue > 0:
                    t_rgb, t_a = self._tongue(a, b, tongue, now)
                    rgb[box], alpha[box] = over(t_rgb, t_a, rgb[box], alpha[box])
                h_rgb, h_a = self._head(a, b, head_t, head_n)
                rgb[box], alpha[box] = over(h_rgb, h_a, rgb[box], alpha[box])
                box_y, box_x = gy.ravel().astype(np.int64), gx.ravel().astype(np.int64)

        out = np.zeros((bh, bw, 4), np.uint8)
        sy, sx = np.concatenate([iy, box_y]), np.concatenate([ix, box_x])
        color = rgb[sy, sx]
        if dead:
            color = color * 0.45 + color.mean(axis=1, keepdims=True) * 0.35 + np.array([0.20, 0.02, 0.02], np.float32)
        out[sy, sx] = to_rgba(color, alpha[sy, sx])
        return out, alpha, (bx0, by0)

    def low_size(self, area):
        """The shadow's size for a board image of `area` pixels, with room
        for a cell past the edge."""
        n = -(-self.g // SHADOW_SCALE)
        return -(-area[1] // SHADOW_SCALE) + n, -(-area[0] // SHADOW_SCALE) + n

    def stamp(self, low, corner, shape, sign):
        """Adds (or with `sign` -1 takes away) a plain cell's coverage to the
        quarter-resolution sum `low`, at the cell's board pixel `corner`."""
        mask = self.mask(shape)
        x, y = int(round(corner[0] / SHADOW_SCALE)), int(round(corner[1] / SHADOW_SCALE))
        low[y:y + mask.shape[0], x:x + mask.shape[1]] += sign * mask

    def shadow(self, area, plain, parts):
        """One soft shadow for the whole snake, at a quarter of the
        resolution: `plain` is the plain cells' coverage, summed as they come
        and go, and the parts drawn this frame add theirs."""
        g, scale = self.g, SHADOW_SCALE
        lw, lh = -(-area[0] // scale), -(-area[1] // scale)
        low = plain.copy()
        for alpha, (bx0, by0) in parts:
            small = shrink(alpha, scale)
            y, x = by0 // scale, bx0 // scale
            low[y:y + small.shape[0], x:x + small.shape[1]] += small
        low = np.clip(low[:lh, :lw], 0, 1)
        radius = max(1, int(round(0.11 * g / scale)))
        low = box_blur(box_blur(low, radius), radius)
        dx, dy = int(round(0.09 * g / scale)), int(round(0.14 * g / scale))
        out = np.zeros((lh, lw, 4), np.uint8)
        out[dy:, dx:, 3] = low[: lh - dy, : lw - dx] * (0.38 * 255) + 0.5
        return out

    def _head(self, a, b, t, n):
        """The head in its own frame: `a` pixels ahead of where the body is
        cut, `b` pixels to its side; `t` and `n` are those two directions on
        screen. A spade, widest at the jaws, under a flattened dome, with the
        eyes bulging out of its outline."""
        g = self.g
        back, front, jaw = -0.42 * g, 0.64 * g, 0.40 * g
        neck = RADIUS * NECK * g
        u = (a - back) / (front - back)
        ahead = np.clip((u - 0.34) / 0.66, 0, 1)
        half = (neck + (jaw - neck) * smoothstep(0.0, 0.34, u)) * (1 - ahead ** 2.3) ** (1 / 2.3)
        half = np.where((u >= 0) & (u <= 1), half, 0.0)
        cover = np.clip(half - np.abs(b) + 0.5, 0, 1) * (half > 0) * smoothstep(0.0, 0.14, u)
        w = np.clip(b / np.maximum(half, 1e-3), -1, 1)
        aw = np.abs(w)
        across = np.sqrt(np.maximum(1 - w * w, 0))
        height = (0.30 - 0.13 * ahead) * g * across
        dy, dx = np.gradient(height)
        normal = np.stack([-dx, -dy, np.ones_like(dx)], -1)
        normal /= np.linalg.norm(normal, axis=-1, keepdims=True)

        color = mix(SKIN["back"], SKIN["flank"], smoothstep(0.5, 0.85, aw))
        color = mix(color, SKIN["lip"], smoothstep(0.78, 0.95, aw) * smoothstep(0.30, 0.45, u))
        # The back's pattern ends on the crown: one dark diamond with a pale
        # rim, and a dark stripe from each eye back to the jaw.
        crown = np.abs(a + 0.08 * g) / (0.27 * g) + np.abs(b) / (0.15 * g)
        color = mix(color, SKIN["rim"], smoothstep(1.18, 1.02, crown) * 0.8)
        color = mix(color, SKIN["blotch"], smoothstep(0.96, 0.84, crown))
        stripe = smoothstep(0.13, 0.05, np.abs(aw - 0.74)) * smoothstep(0.26 * g, 0.12 * g, a) \
            * smoothstep(-0.42 * g, -0.22 * g, a)
        color = mix(color, SKIN["blotch"], stripe * 0.85)
        # Bigger plates than the body's scales.
        k = 0.21 * g
        sa, sb = (a + b) / k, (a - b) / k
        seam = smoothstep(0.36, 0.5, np.maximum(np.abs(sa - np.floor(sa) - 0.5), np.abs(sb - np.floor(sb) - 0.5)))
        color = color * (1 - 0.16 * seam)[..., None]
        rgb = lit(color, normal, 1.25 * (1 - 0.5 * seam), 0.58 + 0.42 * np.sqrt(across))

        for sign in (1, -1):
            # An eye: a dark rim, an amber iris lit from the top left, a slit
            # pupil along the head, and a glint toward the light.
            ea = 0.20 * g
            eb = sign * 0.31 * g
            da, db = a - ea, b - eb
            de = np.hypot(da, db)
            radius = 0.088 * g
            eye = np.clip(radius - de + 0.5, 0, 1)
            sx, sy = da * t[0] + db * n[0], da * t[1] + db * n[1]
            toward = np.clip(-(sx * LIGHT[0] + sy * LIGHT[1]) / radius, -1, 1)
            iris = mix(SKIN["iris"] * 0.45, SKIN["iris"], (toward * 0.5 + 0.5) * smoothstep(radius, radius * 0.3, de))
            pupil = (da / (0.074 * g)) ** 2 + (db / (0.024 * g)) ** 2
            iris = mix(iris, np.zeros(3, np.float32), np.clip((1 - pupil) * 4, 0, 1))
            iris = mix(iris, SKIN["blotch"] * 0.4, smoothstep(radius * 0.72, radius * 0.98, de))
            glint = np.clip(0.024 * g - np.hypot(sx + 0.03 * g, sy + 0.032 * g) + 0.5, 0, 1)
            iris = mix(iris, np.ones(3, np.float32), glint * 0.95)
            rgb = mix(rgb, iris, eye)
            cover = np.maximum(cover, eye)
            nostril = np.clip(0.022 * g - np.hypot(a - 0.53 * g, b - sign * 0.075 * g) + 0.5, 0, 1)
            rgb = mix(rgb, SKIN["blotch"] * 0.35, nostril * 0.9)
        return rgb, cover

    def _tongue(self, a, b, out, now):
        """A forked tongue `out` of the way out (0 to 1), flickering."""
        g = self.g
        flick = math.sin(now * 46.0)
        root = (0.50 * g, 0.0)
        tip = (0.62 * g + 0.36 * g * out, 0.03 * g * flick * out)
        segments = [(root, tip)]
        for sign in (1, -1):
            angle = sign * (0.36 + 0.10 * flick)
            length = 0.17 * g * out
            fork = (tip[0] + length * math.cos(angle), tip[1] + length * math.sin(angle))
            segments.append((tip, fork))
        dist = np.full(a.shape, np.inf, np.float32)
        along = np.zeros(a.shape, np.float32)
        for (ax_, ay_), (bx_, by_) in segments:
            vx, vy = bx_ - ax_, by_ - ay_
            span = max(vx * vx + vy * vy, 1e-6)
            t = np.clip(((a - ax_) * vx + (b - ay_) * vy) / span, 0, 1)
            d = np.hypot(a - (ax_ + t * vx), b - (ay_ + t * vy))
            closer = d < dist
            dist = np.where(closer, d, dist)
            along = np.where(closer, a, along)
        width = 0.03 * g
        cover = np.clip(width - dist + 0.5, 0, 1)
        dark = smoothstep(0.75 * g, 1.15 * g, along)
        color = mix(np.broadcast_to(SKIN["tongue"], a.shape + (3,)), SKIN["tongue"] * 0.4, dark)
        return color, cover

    # The pictures that do not move.

    def ground(self, size, board, cells, dots):
        """The board's border and, with `dots`, a dot at every cell center,
        in grey: the one tone that reads on a dark and a light theme."""
        g = self.g
        w, h = size
        (ox, oy), (cw, chh) = board, cells
        out = np.zeros((h, w, 4), np.uint8)
        out[..., :3] = 128
        if dots:
            gy, gx = np.mgrid[0:g, 0:g].astype(np.float32) + 0.5
            dot = np.clip(0.055 * g - np.hypot(gx - g / 2, gy - g / 2) + 0.5, 0, 1) * 0.28
            out[oy: oy + chh * g, ox: ox + cw * g, 3] = np.tile(dot * 255 + 0.5, (chh, cw))
        pad, corner = 0.18 * g, 0.42 * g
        cx, cy = ox + cw * g / 2, oy + chh * g / 2
        hw, hh = cw * g / 2 + pad - corner, chh * g / 2 + pad - corner
        stroke = max(1.2, g / 26)
        gx = np.arange(w, dtype=np.float32) + 0.5
        qx = np.abs(gx - cx) - hw
        for top in range(0, h, 64):  # a band of rows at a time: the full image in floats is big
            gy = np.arange(top, min(h, top + 64), dtype=np.float32)[:, None] + 0.5
            qy = np.abs(gy - cy) - hh
            outside = (np.hypot(np.maximum(qx, 0), np.maximum(qy, 0))
                       + np.minimum(np.maximum(qx, qy), 0) - corner)
            line = (np.clip(stroke / 2 - np.abs(outside) + 0.5, 0, 1) * 0.5 * 255 + 0.5).astype(np.uint8)
            band = out[top: top + line.shape[0], :, 3]
            np.maximum(band, line, out=band)
        return out

    def food_size(self):
        """The side of a fruit's canvas: room for the fruit and its shadow."""
        return int(1.5 * self.g) | 1

    def food(self, slot):
        """A piece of fruit on its own small canvas, centered, with a contact
        shadow: an apple, a lemon or a plum."""
        g = self.g
        size = self.food_size()
        gy, gx = np.mgrid[0:size, 0:size].astype(np.float32) + 0.5
        x, y = gx - size / 2, gy - size / 2
        if slot == 1:  # a lemon: an ellipse with a nub at each end
            rx, ry = 0.36 * g, 0.27 * g
            q = (x / (rx * (1 + 0.16 * np.exp(-((y / ry) * 3) ** 2)))) ** 2 + (y / ry) ** 2
            base, deep = np.array([0.98, 0.85, 0.20]), np.array([0.70, 0.48, 0.05])
        elif slot == 2:  # a plum
            rx, ry = 0.31 * g, 0.33 * g
            q = (x / rx) ** 2 + ((y - 0.02 * g) / ry) ** 2
            base, deep = np.array([0.52, 0.20, 0.55]), np.array([0.18, 0.04, 0.22])
        else:  # an apple, dimpled at the top
            rx, ry = 0.34 * g, 0.31 * g
            yy = y + 0.05 * g * np.exp(-(x / (0.12 * g)) ** 2) * (y < 0)
            q = (x / rx) ** 2 + ((yy - 0.02 * g) / ry) ** 2
            base, deep = np.array([0.86, 0.13, 0.12]), np.array([0.42, 0.03, 0.05])
        base, deep = base.astype(np.float32), deep.astype(np.float32)
        inside = np.sqrt(np.maximum(1 - q, 0))
        cover = np.clip((1 - q) * min(rx, ry) * 0.5 + 0.5, 0, 1)
        normal = np.stack([x / rx * 0.9, y / ry * 0.9, inside * 1.2 + 0.05], -1)
        normal /= np.linalg.norm(normal, axis=-1, keepdims=True)
        diffuse = np.clip(normal @ LIGHT, 0, 1)
        color = mix(deep, base, np.clip(diffuse * 1.15, 0, 1))
        if slot == 2:  # a plum's bloom: a dusty, wide sheen
            color = mix(color, np.array([0.75, 0.68, 0.85], np.float32), (np.clip(normal @ HALF, 0, 1) ** 6) * 0.35)
        spec = np.clip(normal @ HALF, 0, 1) ** (12 if slot == 2 else 40)
        color = np.clip(color + spec[..., None] * (0.25 if slot == 2 else 0.75), 0, 1)
        rgb, alpha = color, cover
        if slot != 1:
            # A stem, and on the apple a leaf.
            sx, sy = x - 0.03 * g * (y / g + 0.3), y + 0.33 * g
            stem = np.clip(0.028 * g - np.hypot(sx, np.maximum(np.abs(sy) - 0.07 * g, 0)) + 0.5, 0, 1)
            brown = np.broadcast_to(np.array([0.35, 0.22, 0.10], np.float32), rgb.shape)
            rgb, alpha = over(brown, stem, rgb, alpha)
            if slot == 0:
                lx, ly = x - 0.10 * g, y + 0.36 * g
                c, s = math.cos(-0.6), math.sin(-0.6)
                u, v = lx * c - ly * s, lx * s + ly * c
                leaf_q = (u / (0.12 * g)) ** 2 + (v / (0.05 * g)) ** 2
                leaf = np.clip((1 - leaf_q) * 0.05 * g * 0.5 + 0.5, 0, 1)
                leaf_rgb = mix(np.array([0.15, 0.40, 0.10], np.float32), np.array([0.40, 0.70, 0.22], np.float32),
                               np.clip(0.6 - v / (0.05 * g) * 0.4, 0, 1))
                rgb, alpha = over(leaf_rgb, leaf, rgb, alpha)
        sh_q = ((x - 0.06 * g) / (0.36 * g)) ** 2 + ((y - 0.30 * g) / (0.12 * g)) ** 2
        shade = box_blur(np.clip(1 - sh_q, 0, 1) ** 0.5, max(1, int(0.05 * g))) * 0.30
        rgb, alpha = over(rgb, alpha, np.zeros_like(rgb), shade)
        return to_rgba(rgb, alpha)


class Terminal:
    """The pane: raw keys in, text and pictures out."""

    def __init__(self):
        self.fd = sys.stdin.fileno()
        self.saved = termios.tcgetattr(self.fd)
        # A signal writes a byte here, so a resize wakes a wait for keys.
        self.wake, wake = os.pipe()
        os.set_blocking(wake, False)
        signal.set_wakeup_fd(wake)

    def __enter__(self):
        tty.setcbreak(self.fd)  # Ctrl-C still interrupts
        self.write(b"\x1b[?1049h\x1b[?25l\x1b[2J")
        return self

    def __exit__(self, *exc):
        self.write(b"\x1b_Ga=d,d=A,q=2\x1b\\\x1b[2J\x1b[?25h\x1b[?1049l")
        termios.tcsetattr(self.fd, termios.TCSADRAIN, self.saved)

    def write(self, data):
        view = memoryview(data)
        while view:
            try:
                view = view[os.write(1, view):]
            except InterruptedError:
                pass
            except BlockingIOError:  # a program before this one left the terminal non-blocking
                select.select([], [1], [])

    def size(self):
        """Columns, rows and a cell's size in pixels (0, 0 if unknown)."""
        rows, cols, xpix, ypix = struct.unpack("HHHH", fcntl.ioctl(1, termios.TIOCGWINSZ, b"\0" * 8))
        if xpix and ypix and cols and rows:
            return cols, rows, (xpix / cols, ypix / rows)
        return cols, rows, (0, 0)

    def read(self, timeout):
        """Bytes typed within `timeout` seconds, or b"" once a signal or the
        time is up."""
        ready, _, _ = select.select([self.fd, self.wake], [], [], max(0.0, timeout))
        if self.wake in ready:
            os.read(self.wake, 512)
        return os.read(self.fd, 4096) if self.fd in ready else b""

    def ask(self, query, timeout=1.5):
        """Sends `query` and a device attributes request, and returns what
        comes back up to the attributes answer, which every terminal sends:
        an answer that has not come by then is not coming."""
        self.write(query + b"\x1b[c")
        got, end = b"", time.monotonic() + timeout
        while time.monotonic() < end:
            got += self.read(end - time.monotonic())
            at = got.find(b"\x1b[?")
            if at >= 0 and got.find(b"c", at) >= 0:
                break
        return got


class Sender:
    """Puts pictures on the pane, by shared memory where the terminal can
    read it and in the stream where it cannot."""

    def __init__(self, terminal):
        self.term = terminal
        self.shared = False
        self.serial = 0
        self.recent = deque(maxlen=32)
        self.cell = (0, 0)

    def _share(self, data):
        self.serial += 1
        name = f"/bsnake{os.getpid()}.{self.serial}"  # macOS allows 31 bytes
        fd = _posixshmem.shm_open(name, os.O_CREAT | os.O_EXCL | os.O_RDWR, 0o600)
        try:
            os.ftruncate(fd, len(data))
            with mmap.mmap(fd, len(data)) as view:
                view[:] = data
        finally:
            os.close(fd)
        self.recent.append(name)
        return name

    def probe(self):
        """Whether the terminal shows pictures, and whether it reads this
        process's shared memory; also asks for the cell size, in case the
        window size does not carry it."""
        query = b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\"
        shared = False
        if _posixshmem is not None:
            try:
                name = self._share(b"\0\0\0")
                query += (b"\x1b_Gi=32,s=1,v=1,a=q,t=s,f=24,S=3;"
                          + base64.b64encode(name.encode()) + b"\x1b\\")
                shared = True
            except OSError:
                pass
        answer = self.term.ask(query + b"\x1b[16t")
        self.shared = shared and b"Gi=32;OK" in answer
        self.forget()
        at = answer.find(b"\x1b[6;")
        if at >= 0:
            try:
                h, w = answer[at + 4: answer.index(b"t", at)].split(b";")
                self.cell = (int(w), int(h))
            except ValueError:
                pass
        return b"Gi=31;OK" in answer

    def forget(self):
        """Removes the shared memory objects the terminal may not have taken;
        the ones it took are gone already."""
        while self.recent:
            try:
                _posixshmem.shm_unlink(self.recent.popleft())
            except OSError:
                pass

    def failed(self):
        """A picture came back with an error: the stream from here on."""
        if self.shared:
            self.shared = False
            self.forget()

    def image(self, image_id, rgba, at=None, z=0, cell=None, cells=None):
        """Sends `rgba` as image `image_id` and, with `at`, shows it with its
        top left corner at that pane pixel, stretched over `cells` (columns,
        rows) if given."""
        h, w = rgba.shape[:2]
        data = rgba.tobytes()
        keys = f"i={image_id},q=1,f=32,s={w},v={h}"
        out = []
        if at is None:
            keys = "a=t," + keys
        else:
            place, offset = Sender._place(at, cell)
            out.append(place)
            keys = f"a=T,{keys},C=1,z={z},{offset}"
            if cells:
                keys += f",c={cells[0]},r={cells[1]}"
        if self.shared:
            name = self._share(data)
            out.append(f"\x1b_G{keys},t=s,S={len(data)};".encode()
                       + base64.b64encode(name.encode()) + b"\x1b\\")
            return b"".join(out)
        payload = base64.b64encode(zlib.compress(data, 1))
        chunks = [payload[i:i + 4096] for i in range(0, len(payload), 4096)] or [b""]
        for i, chunk in enumerate(chunks):
            more = int(i < len(chunks) - 1)
            head = f"{keys},o=z,m={more}" if i == 0 else f"m={more}"
            out.append(f"\x1b_G{head};".encode() + chunk + b"\x1b\\")
        return b"".join(out)

    @staticmethod
    def _place(at, cell):
        """The cursor move to the cell holding pane pixel `at`, and the
        offset keys for where in that cell the picture starts."""
        cw, ch = cell
        col, row = int(at[0] // cw), int(at[1] // ch)
        off_x, off_y = int(at[0] - col * cw), int(at[1] - row * ch)
        return f"\x1b[{row + 1};{col + 1}H".encode(), f"X={off_x},Y={off_y}"

    @staticmethod
    def put(image_id, placement, at, z, cell):
        """Shows a stored image again, as placement `placement`."""
        place, offset = Sender._place(at, cell)
        return place + f"\x1b_Ga=p,i={image_id},p={placement},q=2,C=1,z={z},{offset}\x1b\\".encode()

    @staticmethod
    def put_part(image_id, placement, at, z, cell, y, w, h):
        """`put` of the `w` × `h` part of a stored image at row `y`, all but
        its column: the caller ends the command with `column`, the one thing
        that changes from frame to frame."""
        place, offset = Sender._place(at, cell)
        return place + f"\x1b_Ga=p,i={image_id},p={placement},q=2,C=1,z={z},{offset},y={y},w={w},h={h},x=".encode()

    @staticmethod
    def column(x):
        return f"{x}\x1b\\".encode()

    @staticmethod
    def remove(image_id, placement):
        """Takes a placement away and keeps the image."""
        return f"\x1b_Ga=d,d=i,i={image_id},p={placement},q=2\x1b\\".encode()

    @staticmethod
    def drop(image_id):
        """Takes an image away, and its placements."""
        return f"\x1b_Ga=d,d=I,i={image_id},q=2\x1b\\".encode()


class Board:
    """One game on the pane: where it sits, and what of it is on screen."""

    def __init__(self, args, cols, rows, cell, painters):
        self.cols, self.rows, self.cell = cols, rows, cell
        cw, ch = cell
        self.g = g = max(8, int(round(ch * args.size)))
        # The pictures cover every row but the top and the bottom one, which
        # keep the title, the score and the hints as text.
        self.top = ch
        self.area = (int(cols * cw), int((rows - 2) * ch))
        margin = max(cw, 0.45 * g)
        w = int((self.area[0] - 2 * margin) // g)
        h = int((self.area[1] - 2 * margin) // g)
        self.ok = rows > 2 and w >= MIN_W and h >= MIN_H
        if not self.ok:
            return
        self.origin = ((self.area[0] - w * g) // 2, (self.area[1] - h * g) // 2)
        self.game = Game(w, h, args.length, args.food, args.seed)
        if g not in painters:
            painters.clear()  # one size at a time: the atlas is per size
            painters[g] = Painter(g)
        self.painter = painters[g]
        self.track = Track(self.game, self.painter, self.origin)
        self.shown = {}  # cell -> (slot, placement) of the fruit on screen
        self.eaten = []  # (path index, cell) of fruit eaten, until the mouth gets there
        self.bulges = []  # times a piece of fruit went down
        self.tiles = {}  # placement -> its command, less the part of the atlas, for the plain body on screen
        self.phase = None  # the skin's position the tiles show
        self.plain = np.zeros(self.painter.low_size(self.area), np.float32)
        self.stamped = {}  # cell index -> (corner, shape) summed into `plain`
        self.live = set()  # ids of the per-frame pictures on screen

    def pane_px(self, board_px):
        return (board_px[0], board_px[1] + self.top)

    def statics(self, sender, dots, atlas):
        """The ground, shown; the three fruit and, with `atlas`, the plain
        body's shapes, stored to be shown."""
        ground = self.painter.ground(self.area, self.origin, (self.game.w, self.game.h), dots)
        out = [sender.image(GROUND_ID, ground, (0, self.top), GROUND_Z, self.cell)]
        out += [sender.image(FOOD_ID + slot, self.painter.food(slot)) for slot in range(3)]
        if atlas:
            out.append(sender.image(ATLAS_ID, self.painter.atlas()))
        return b"".join(out)

    def ate(self):
        """The game just ate the fruit under the head."""
        self.eaten.append((self.track.head, self.game.snake[0]))

    def busy(self):
        return bool(self.bulges or self.eaten)

    def fruit(self, p_head, now):
        """Shows new fruit, and takes eaten fruit away once the snake's mouth
        closes on it on screen (the game ate it a cell and a half earlier)."""
        out = []
        for index, cell in list(self.eaten):
            if p_head >= index - 0.85:
                self.eaten.remove((index, cell))
                slot, placement = self.shown.pop(cell)
                out.append(Sender.remove(FOOD_ID + slot, placement))
                self.bulges.append(now)
        g = self.g
        size = self.painter.food_size()
        for cell, slot in self.game.food.items():
            if cell not in self.shown:
                placement = 1 + cell[0] + cell[1] * self.game.w
                self.shown[cell] = (slot, placement)
                corner = (self.origin[0] + (cell[0] + 0.5) * g - size / 2,
                          self.origin[1] + (cell[1] + 0.5) * g - size / 2)
                out.append(Sender.put(FOOD_ID + slot, placement, self.pane_px(corner), FOOD_Z, self.cell))
        return b"".join(out)

    def frame(self, sender, s, now, tongue):
        """Everything that changes on screen at `s` of the way through the
        current step."""
        track, painter, g = self.track, self.painter, self.g
        dead = self.game.over
        p_head, p_tail = track.ends(s)
        self.bulges = [t0 for t0 in self.bulges if now - t0 < SWALLOW]
        reach = max(p_head - p_tail, 1.0)
        bulges = [0.6 + (now - t0) / SWALLOW * reach for t0 in self.bulges]
        # The skin moves with the body, in steps of a PHASES-th of a cell, so
        # the parts drawn here match the plain body's stored shapes.
        tick = round(p_head * PHASES)
        phase = tick / PHASES
        pieces = track.pieces(p_head, p_tail)
        lo, hi = track.steady()
        steady = [] if dead else [m for m in pieces if lo <= m <= hi]
        if steady:
            tail_part = [m for m in pieces if m < lo]
            head_part = [m for m in pieces if m > hi]
        else:
            tail_part, head_part = [], pieces
        out = [self.fruit(p_head, now)]

        # The plain body: a placement of the right part of the atlas per cell.
        # A cell's shape does not change while it is plain, so only the skin's
        # position is new from frame to frame.
        wanted = {}
        for m in steady:
            x, y = track.cells[m]
            wanted[1 + x + y * self.game.w] = m
        moved = tick % PHASES != self.phase
        self.phase = tick % PHASES
        column = Sender.column(self.phase * g)
        for placement in self.tiles.keys() - wanted.keys():
            out.append(Sender.remove(ATLAS_ID, placement))
            del self.tiles[placement]
        for placement, m in wanted.items():
            command = self.tiles.get(placement)
            if command is None:
                command = Sender.put_part(ATLAS_ID, placement, self.pane_px(track.corner(m)), ATLAS_Z,
                                          self.cell, SHAPE_ROW[track.shape(m)] * g, g, g)
                self.tiles[placement] = command
            elif not moved:
                continue
            out.append(command + column)

        # The plain cells' share of the shadow, as cells come and go.
        plain = set(steady)
        for m in [m for m in self.stamped if m not in plain]:
            painter.stamp(self.plain, *self.stamped.pop(m), -1)
        for m in steady:
            if m not in self.stamped:
                self.stamped[m] = (track.corner(m), track.shape(m))
                painter.stamp(self.plain, *self.stamped[m], 1)

        # The parts that change shape: head and neck, tail, bulges.
        shown, shadows = set(), []

        def show(image_id, z, part):
            if part is None:
                return
            rgba, alpha, corner = part
            out.append(sender.image(image_id, rgba, self.pane_px(corner), z, self.cell))
            shown.add(image_id)
            shadows.append((alpha, corner))

        ends = (p_head, p_tail)
        show(HEAD_ID, HEAD_Z, painter.part(track, head_part, ends, phase, bulges, self.area,
                                            head=(0.0 if dead else tongue, now), dead=dead))
        show(TAIL_ID, TAIL_Z, painter.part(track, tail_part, ends, phase, bulges, self.area, dead=dead))
        for j, at in enumerate(bulges[:BULGES]):
            middle = p_head - at
            near = [m for m in steady if abs(m - middle) <= 2]
            if near:
                part = painter.part(track, near, ends, phase, bulges, self.area)
                if part:
                    rgba, _, corner = part
                    out.append(sender.image(BULGE_ID + j, rgba, self.pane_px(corner), BULGE_Z, self.cell))
                    shown.add(BULGE_ID + j)
        for image_id in self.live - shown:
            out.append(Sender.drop(image_id))
        self.live = shown

        shade = painter.shadow(self.area, self.plain, shadows)
        out.append(sender.image(SHADOW_ID, shade, (0, self.top), SHADOW_Z, self.cell,
                                cells=(self.cols, self.rows - 2)))
        return b"".join(out)


def hud(board, cols, rows, auto, hints):
    """The text: title and score on the top row, hints on the bottom one and,
    when the game is over, a banner across the middle."""
    middle = rows // 2 + 1
    out = [b"\x1b[0m\x1b[1;1H\x1b[2K", f"\x1b[{rows};1H\x1b[2K\x1b[{middle};1H\x1b[2K".encode()]
    if board is None or not board.ok:
        out.append(b"\x1b[1;1Htoo small")
        return b"".join(out)
    game = board.game
    title = " snake "
    status = f" {'autopilot · ' if auto else ''}score {game.score} · length {len(game.snake)} "
    if len(title) + len(status) + 4 <= cols:
        out.append(f"\x1b[1;2H\x1b[1m{title}\x1b[0m".encode())
        out.append(f"\x1b[1;{cols - len(status)}H\x1b[36m{status}\x1b[0m".encode())
    if hints:
        text = " ←↑↓→ move · space pause · a autopilot · r restart · q quit "
        if len(text) + 4 <= cols:
            out.append(f"\x1b[{rows};{(cols - len(text)) // 2 + 1}H\x1b[90m{text}\x1b[0m".encode())
    if game.over:
        text = f"  game over · score {game.score} · r restart · q quit  "
        if len(text) <= cols - 2:
            out.append(f"\x1b[{middle};{(cols - len(text)) // 2 + 1}H\x1b[1;7;31m{text}\x1b[0m".encode())
    return b"".join(out)


def tongue_out(t):
    """How far the tongue is out, `t` seconds into a flick: out, a quiver,
    back in; 0 outside the flick."""
    if t < 0 or t > FLICK:
        return 0.0
    if t < 0.09:
        return t / 0.09
    if t < FLICK - 0.11:
        return 1.0
    return (FLICK - t) / 0.11


def keys_in(data):
    """Splits what was typed into keys; an answer to a picture is a key too,
    so it can be told apart."""
    while data:
        if data.startswith(b"\x1b_G"):
            end = data.find(b"\x1b\\")
            end = len(data) if end < 0 else end + 2
        elif data.startswith(b"\x1b["):
            end = 2
            while end < len(data) and not 0x40 <= data[end] <= 0x7E:
                end += 1
            end += 1
        elif data.startswith(b"\x1bO"):
            end = 3
        else:
            end = 1
        yield data[:end]
        data = data[end:]


def run(term, args, stats):
    sender = Sender(term)
    if not sender.probe():
        return ("this terminal does not show kitty graphics; "
                "tools/snake.py plays the same game in plain text")
    if args.transport == "stream":
        sender.failed()
    elif args.transport == "shm" and not sender.shared:
        return "the terminal cannot read this process's shared memory; try --transport stream"

    fresh = [True]
    signal.signal(signal.SIGWINCH, lambda *_: fresh.__setitem__(0, True))
    rng = random.Random()
    auto, hints, paused = args.auto, not args.auto, False
    tick, gap = 1.0 / args.speed, 1.0 / args.fps
    board = None
    painters = {}  # the cell size's painter, which keeps the atlas once drawn
    atlas_at = None  # the cell size of the atlas the terminal has
    atlas_out = None  # the cell size of an atlas in `out`, not written yet
    try:
        while True:
            now = time.monotonic()
            out = b""
            if fresh[0]:
                # A new game: at the start, after a resize, and on r. What it
                # replaces goes in the same update as its first frame; the
                # stored pictures stay, and are replaced by id.
                fresh[0] = False
                cols, rows, cell = term.size()
                if not cell[0]:
                    cell = sender.cell if sender.cell[0] else (8, 16)
                board = Board(args, cols, rows, cell, painters)
                started, paused, over_at = auto, False, None
                due, stepped = now + tick, now
                flick_at, last, drawn = now + 0.8, 0.0, False
                out = b"\x1b_Ga=d,d=a,q=2\x1b\\\x1b[2J"
                # Another resize can come before `out` is written, and then
                # `out` is dropped: the atlas counts as sent once written.
                atlas_out = board.g if board.ok and board.g != atlas_at else None
                if board.ok:
                    out += board.statics(sender, not args.no_dots, atlas=atlas_out is not None)
                out += hud(board, cols, rows, auto, hints)
                if not board.ok:
                    term.write(out)
                    out = b""

            # Sleep until a key, the next step or the next frame.
            wake = now + 1.0
            if board.ok and not paused:
                game = board.game
                if started and not game.over:
                    wake = min(wake, due)
                busy = started and not game.over or board.busy() or not drawn
                wake = min(wake, last + gap if busy or 0 <= now - flick_at <= FLICK else flick_at)
            text = False
            for key in keys_in(term.read(wake - now) if not out else b""):
                if key.startswith(b"\x1b_G"):
                    if b";OK" not in key and sender.shared:
                        # Something did not arrive: send it all again in the stream.
                        sender.failed()
                        if board.ok:
                            board.shown.clear()
                            board.tiles.clear()
                            board.phase = None
                            out += b"\x1b_Ga=d,d=a,q=2\x1b\\" + board.statics(sender, not args.no_dots, atlas=True)
                            atlas_out, drawn = board.g, False
                elif key == b"q":
                    return None
                elif key == b"?":
                    hints, text = not hints, True
                elif not board.ok:
                    pass
                elif key == b"r":
                    fresh[0] = True
                elif key == b" " and started and not board.game.over:
                    paused = not paused
                    if paused:
                        paused_at = time.monotonic()
                    else:
                        late = time.monotonic() - paused_at
                        due, stepped = due + late, stepped + late
                elif key == b"a":
                    auto, hints, text = not auto, False, True
                elif key in KEYS and not auto:
                    board.game.turn(KEYS[key])
                    if hints:
                        hints, text = False, True
                else:
                    continue
                if (auto or key in KEYS) and not started and not paused:
                    started = True
                    due, stepped = time.monotonic() + tick, time.monotonic()
            if fresh[0]:
                continue
            if not board.ok:
                if text:
                    term.write(hud(board, cols, rows, auto, hints))
                continue

            game, track = board.game, board.track
            now = time.monotonic()
            if game.over and auto and over_at and now - over_at > 1.5:
                fresh[0] = True
                continue
            if started and not paused and not game.over and now >= due:
                if auto:
                    game.autopilot()
                score = game.score
                game.step()
                if game.over:
                    track.hold()
                    over_at, drawn = now, False
                else:
                    track.advance(game)
                    if game.score != score:
                        board.ate()
                text = text or game.score != score or game.over
                stepped, due = due, due + tick
                if due < now:  # a slow frame: carry on from now, do not race
                    stepped, due = now, now + tick
            if text:
                out += hud(board, cols, rows, auto, hints)

            # The frame, and whatever else is waiting to be written.
            if not paused:
                flick = now - flick_at
                if flick > FLICK:
                    flick_at = now + rng.uniform(1.4, 4.0)
                moving = started and not game.over
                due_frame = moving or not drawn or board.busy() or (0 <= flick <= FLICK and not game.over)
                if due_frame and now >= last + gap:
                    s = min(max((now - stepped) / tick, 0.0), 1.0) if moving else 1.0
                    out = b"".join([b"\x1b[?2026h", out, board.frame(sender, s, now, tongue_out(flick)),
                                    b"\x1b[?2026l"])
                    last, drawn = now, True
                    stats["frames"] = stats.get("frames", 0) + 1
                    stats.setdefault("first", now)
                    stats["last"] = now
                    stats["shared"] = sender.shared
            if out:
                term.write(out)
                if atlas_out is not None:
                    atlas_at, atlas_out = atlas_out, None
    finally:
        # Wait for the terminal to have read every picture already sent
        # before taking back the shared memory it has not.
        term.ask(b"", timeout=1.0)
        sender.forget()


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--auto", action="store_true", help="the snake plays itself")
    parser.add_argument("--length", type=int, help="starting length (default: three lanes)")
    parser.add_argument("--food", type=int, help="pieces of fruit on the board")
    parser.add_argument("--speed", type=float, default=8, help="steps per second (default 8)")
    parser.add_argument("--seed", type=int, help="the same board every run")
    parser.add_argument("--size", type=float, default=1.3,
                        help="a cell's side in text rows (default 1.3)")
    parser.add_argument("--fps", type=float, default=60, help="frames per second at most (default 60)")
    parser.add_argument("--no-dots", action="store_true", help="a plain background")
    parser.add_argument("--transport", choices=("auto", "shm", "stream"), default="auto",
                        help="how pictures reach the terminal (default: shared memory if it can)")
    parser.add_argument("--stats", action="store_true", help="say on exit how many frames a second it drew")
    args = parser.parse_args()
    if args.speed <= 0 or args.fps <= 0 or args.size <= 0:
        parser.error("--speed, --fps and --size must be above 0")
    if not os.isatty(0) or not os.isatty(1):
        parser.error("needs a terminal")
    stats = {}
    try:
        with Terminal() as term:
            problem = run(term, args, stats)
    except KeyboardInterrupt:
        problem = None
    if args.stats and stats.get("frames", 0) > 1:
        span = stats["last"] - stats["first"]
        print(f"{stats['frames']} frames in {span:.1f} s, {(stats['frames'] - 1) / max(span, 1e-6):.1f} a second, "
              f"through {'shared memory' if stats['shared'] else 'the stream'}", file=sys.stderr)
    if problem:
        sys.exit(problem)


if __name__ == "__main__":
    main()
