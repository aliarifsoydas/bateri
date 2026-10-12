#!/usr/bin/env python3
"""A snake game that fills the pane it runs in — a prop for screenshots.

Usage:

    python3 tools/snake.py              # play: arrows, WASD or hjkl
    python3 tools/snake.py --auto       # the snake plays itself
    python3 tools/snake.py --length 120 # start longer

By hand the snake waits for the first arrow. Keys: space pauses (and draws
nothing, so the frame still looks like play), a toggles the autopilot, r
restarts, ? shows the key hints again, q quits.

The board is the whole pane, every time: a cell is two columns wide so it
comes out square, the border hugs the pane, and a resize starts a new game
that fits. The snake starts already grown, coiled in the middle with its head
pointing at open space, and a few pieces of food are scattered around, so
the very first frame is worth a screenshot. Every color is one of the
theme's sixteen and the background is the theme's own, so the game follows
whatever theme the terminal shows.

Standard library only (curses).
"""

import argparse
import curses
import locale
import os
import random
import time
from collections import deque

RIGHT, LEFT, UP, DOWN = (1, 0), (-1, 0), (0, -1), (0, 1)

# `a` is the autopilot toggle, so WASD's left is the arrow or `h`.
KEYS = {
    curses.KEY_RIGHT: RIGHT, ord("d"): RIGHT, ord("l"): RIGHT,
    curses.KEY_LEFT: LEFT, ord("h"): LEFT,
    curses.KEY_UP: UP, ord("w"): UP, ord("k"): UP,
    curses.KEY_DOWN: DOWN, ord("s"): DOWN, ord("j"): DOWN,
}

# The smallest board that still plays: below it the pane shows a note.
MIN_W, MIN_H = 8, 5


class Game:
    """The rules, with no screen: the board is `w` × `h` cells."""

    def __init__(self, w, h, length=None, food=None, seed=None):
        self.w, self.h = w, h
        self.rng = random.Random(seed)
        self.snake = deque(self._coil(length))  # head first
        self.body = set(self.snake)
        self.heading = RIGHT
        self.turns = deque()
        self.grow = 0
        self.score = 0
        self.over = False
        self.food = {}  # cell -> color slot
        count = food if food is not None else max(3, min(12, w * h // 200))
        for _ in range(count):
            self._drop()

    def _coil(self, length):
        """A boustrophedon in the middle of the board, head at the top lane's
        right end: three lanes by default, as many as asked and as fit."""
        span = max(3, int(self.w * 0.55))
        lanes_fit = max(1, (self.h + 1) // 2)
        if length is None:
            length = 3 * span + 2
        length = max(3, min(length, lanes_fit * (span + 1) - 1, self.w * self.h // 2))
        lanes = -(-(length + 1) // (span + 1))
        x0 = (self.w - span) // 2
        y0 = max(0, (self.h - (2 * lanes - 1)) // 2)
        cells = []
        for lane in range(lanes):
            y = y0 + 2 * lane
            xs = range(x0 + span - 1, x0 - 1, -1) if lane % 2 == 0 else range(x0, x0 + span)
            cells.extend((x, y) for x in xs)
            end = x0 if lane % 2 == 0 else x0 + span - 1
            if y + 1 < self.h:
                cells.append((end, y + 1))
        return cells[:length]

    def _drop(self):
        free = [(x, y) for x in range(self.w) for y in range(self.h)
                if (x, y) not in self.body and (x, y) not in self.food]
        if free:
            self.food[self.rng.choice(free)] = self.rng.randrange(3)

    def turn(self, direction):
        last = self.turns[-1] if self.turns else self.heading
        if direction != last and direction != (-last[0], -last[1]) and len(self.turns) < 2:
            self.turns.append(direction)

    def step(self):
        if self.over:
            return
        if self.turns:
            self.heading = self.turns.popleft()
        hx, hy = self.snake[0]
        head = (hx + self.heading[0], hy + self.heading[1])
        tail = self.snake[-1]
        inside = 0 <= head[0] < self.w and 0 <= head[1] < self.h
        if not inside or (head in self.body and not (head == tail and self.grow == 0)):
            self.over = True
            return
        if self.grow:
            self.grow -= 1
        else:
            self.body.discard(self.snake.pop())
        self.snake.appendleft(head)
        self.body.add(head)
        if head in self.food:
            del self.food[head]
            self.score += 1
            self.grow += 1
            self._drop()

    # The autopilot: the shortest way to food that still leaves a way to the
    # tail, else the way to the tail, else the move with the most room.

    def _neighbors(self, cell):
        x, y = cell
        for d in (RIGHT, LEFT, UP, DOWN):
            n = (x + d[0], y + d[1])
            if 0 <= n[0] < self.w and 0 <= n[1] < self.h:
                yield d, n

    def _path(self, start, goals, blocked):
        """First step of a shortest path from `start` to any of `goals`."""
        seen = {start: None}
        queue = deque([start])
        while queue:
            cell = queue.popleft()
            if cell in goals and cell != start:
                while seen[cell][1] != start:
                    cell = seen[cell][1]
                return seen[cell][0]
            for d, n in self._neighbors(cell):
                if n not in seen and (n not in blocked or n in goals):
                    seen[n] = (d, cell)
                    queue.append(n)
        return None

    def _room(self, start, blocked):
        seen, stack = {start}, [start]
        while stack:
            for _, n in self._neighbors(stack.pop()):
                if n not in seen and n not in blocked:
                    seen.add(n)
                    stack.append(n)
        return len(seen)

    def _leaves_a_way_out(self, cell):
        """Whether the head can still reach the tail after moving to `cell`."""
        after = [cell] + list(self.snake)
        if not (self.grow or cell in self.food):
            after.pop()
        if len(after) < 4:
            return True
        return self._path(cell, {after[-1]}, set(after) - {after[-1]}) is not None

    def autopilot(self):
        head, tail = self.snake[0], self.snake[-1]
        blocked = set(self.body) if self.grow else self.body - {tail}
        self.turns.clear()
        to_food = self._path(head, set(self.food), blocked)
        if to_food and self._leaves_a_way_out((head[0] + to_food[0], head[1] + to_food[1])):
            self.turn(to_food)
            return
        to_tail = self._path(head, {tail}, blocked)
        if to_tail:
            self.turn(to_tail)
            return
        room = [(self._room(n, blocked | {head}), d) for d, n in self._neighbors(head)
                if n not in blocked]
        if room:
            self.turn(max(room)[1])


class Screen:
    """Draws a `Game` onto the whole pane."""

    def __init__(self, scr):
        self.scr = scr
        curses.use_default_colors()
        bright = curses.COLORS >= 16
        pairs = {
            "body": curses.COLOR_GREEN,
            "head": 10 if bright else curses.COLOR_GREEN,
            "food0": curses.COLOR_RED,
            "food1": curses.COLOR_YELLOW,
            "food2": curses.COLOR_MAGENTA,
            "frame": 8 if bright else -1,
            "accent": curses.COLOR_CYAN,
            "alert": curses.COLOR_RED,
        }
        self.color = {}
        for i, (name, fg) in enumerate(pairs.items(), start=1):
            curses.init_pair(i, fg, -1)
            self.color[name] = curses.color_pair(i)
        if not bright:
            self.color["frame"] |= curses.A_DIM

    def put(self, y, x, text, attr=0):
        try:
            self.scr.addstr(y, x, text, attr)
        except curses.error:
            pass  # the bottom-right cell: written, but the cursor cannot advance

    def board_size(self):
        rows, cols = self.scr.getmaxyx()
        return (cols - 2) // 2, rows - 2

    def draw(self, game, hints, auto, dots):
        scr = self.scr
        scr.erase()
        rows, cols = scr.getmaxyx()
        w, h = game.w, game.h
        right = 2 * w + 1
        frame = self.color["frame"]

        self.put(0, 0, "╭" + "─" * (right - 1) + "╮", frame)
        for y in range(1, h + 1):
            self.put(y, 0, "│", frame)
            self.put(y, right, "│", frame)
        self.put(h + 1, 0, "╰" + "─" * (right - 1) + "╯", frame)

        title = " snake "
        status = f" {'autopilot · ' if auto else ''}score {game.score} · length {len(game.snake)} "
        if len(title) + len(status) + 4 <= right:
            self.put(0, 2, title, curses.A_BOLD)
            self.put(0, right - 1 - len(status), status, self.color["accent"])
        if hints:
            text = " ←↑↓→ move · space pause · a autopilot · r restart · q quit "
            if len(text) + 4 <= right:
                self.put(h + 1, (right + 1 - len(text)) // 2, text, frame)

        if dots:
            for y in range(h):
                line = "· " * w
                self.put(y + 1, 1, line, frame)

        for (x, y), slot in game.food.items():
            self.put(y + 1, 2 * x + 1, "██", self.color[f"food{slot}"])
        snake = list(game.snake)
        for x, y in snake[1:]:
            self.put(y + 1, 2 * x + 1, "██", self.color["body"])
        hx, hy = snake[0]
        eyes = {RIGHT: " :", LEFT: ": ", UP: "••", DOWN: "••"}[game.heading]
        head = self.color["alert"] if game.over else self.color["head"]
        self.put(hy + 1, 2 * hx + 1, eyes, head | curses.A_REVERSE | curses.A_BOLD)

        if game.over:
            text = f"  game over · score {game.score} · r restart · q quit  "
            if len(text) <= right - 1:
                self.put(h // 2 + 1, (right + 1 - len(text)) // 2, text,
                         self.color["alert"] | curses.A_REVERSE | curses.A_BOLD)
        scr.refresh()

    def too_small(self):
        self.scr.erase()
        self.put(0, 0, "too small")
        self.scr.refresh()


def run(scr, args):
    try:
        curses.curs_set(0)
    except curses.error:
        pass
    screen = Screen(scr)
    scr.keypad(True)

    def new_game():
        w, h = screen.board_size()
        if w < MIN_W or h < MIN_H:
            return None
        return Game(w, h, args.length, args.food, args.seed)

    game = new_game()
    auto = args.auto
    hints = not auto
    paused = False
    tick = 1.0 / args.speed
    due = time.monotonic()
    over_at = None
    # By hand the snake waits for the first arrow, so the opening frame holds
    # still for as long as the screenshot takes.
    started = auto

    while True:
        if game is None:
            screen.too_small()
        else:
            screen.draw(game, hints, auto, not args.no_dots)
        scr.timeout(max(0, int((due - time.monotonic()) * 1000)))
        key = scr.getch()
        if key == curses.KEY_RESIZE:
            game, over_at, started = new_game(), None, auto
            continue
        if key != -1:
            if key == ord("q"):
                return
            if key == ord("?"):
                hints = not hints
            elif game is None:
                pass
            elif key == ord(" "):
                paused = not paused
            elif key == ord("a"):
                auto = not auto
                started = started or auto
                hints = False
            elif key == ord("r"):
                game, over_at, started = new_game(), None, auto
            elif key in KEYS and not auto:
                game.turn(KEYS[key])
                started = True
                hints = False
            if time.monotonic() < due:
                continue
        due = time.monotonic() + tick
        if game is None or paused or not started:
            continue
        if game.over:
            if auto and over_at and time.monotonic() - over_at > 1.5:
                game, over_at = new_game(), None
            continue
        if auto:
            game.autopilot()
        game.step()
        if game.over:
            over_at = time.monotonic()


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--auto", action="store_true", help="the snake plays itself")
    parser.add_argument("--length", type=int, help="starting length (default: three lanes)")
    parser.add_argument("--food", type=int, help="pieces of food on the board")
    parser.add_argument("--speed", type=float, default=9, help="steps per second (default 9)")
    parser.add_argument("--seed", type=int, help="the same board every run")
    parser.add_argument("--no-dots", action="store_true", help="a plain background")
    args = parser.parse_args()
    if args.speed <= 0:
        parser.error("--speed must be above 0")
    try:
        locale.setlocale(locale.LC_ALL, "")
    except locale.Error:
        pass  # a LANG the system lacks: curses still draws, only wide text may slip
    os.environ.setdefault("ESCDELAY", "25")
    try:
        curses.wrapper(run, args)
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
