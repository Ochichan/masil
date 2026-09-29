#!/usr/bin/env python3
"""Floating groups: panes split inside a floating pane move, resize and raise
together. Checks the layout tree after every step against real servers."""
import json
import random
import unittest

from test_compatibility import BASELINE, RMUX, Server

PANES = '#{pane_id} #{pane_floating_flag} #{pane_left} #{pane_top} #{pane_width} #{pane_height}'


def geometry(cell):
    """A cell's shape and position without active or last pane marks."""
    shape = {k: cell[k] for k in ('t', 'x', 'y', 'w', 'h')}
    if 'c' in cell:
        shape['c'] = [geometry(child) for child in cell['c']]
    return shape


def in_tiling(cell):
    if 'z' in cell:
        return False
    return cell['t'] == 'p' or any(in_tiling(child) for child in cell['c'])


def comparable(cell):
    """Drop the geometry upstream tmux does not keep for float-only nodes."""
    cell = dict(cell)
    if cell['t'] != 'p' and 'z' not in cell and not in_tiling(cell):
        for key in 'xywh':
            cell.pop(key)
    if 'c' in cell:
        cell['c'] = [comparable(child) for child in cell['c']]
    return cell


class Layout:
    """window_layout (v2 JSON) with the geometry rules a valid tree obeys."""

    def __init__(self, text):
        self.root = json.loads(text)['L']
        self.leaves = {}
        self.groups = []
        self.problems = []
        self.main_problems = []
        self.walk(self.root, None)

    def walk(self, cell, group):
        if 'z' in cell:
            if group is not None:
                self.problems.append(f'float root inside a group: {cell}')
            group = cell
            if cell['t'] != 'p':
                self.groups.append(cell)
        if cell['t'] == 'p':
            self.leaves[cell['I']] = (cell, group)
            return
        children = cell['c']
        if len(children) < 2:
            self.problems.append(f'node with {len(children)} children: {cell}')
        # Inside a group every child is tiled; in the main tree floats, and
        # nodes holding only floats, are skipped.
        tiled = children if group is not None else [c for c in children if in_tiling(c)]
        if tiled:
            self.check_tiling(cell, tiled, self.problems if group is not None else self.main_problems)
        for child in children:
            self.walk(child, group)

    def check_tiling(self, node, children, problems):
        across, along = ('x', 'w'), ('y', 'h')
        if node['t'] == 'v':
            across, along = along, across
        position = node[across[0]]
        for child in children:
            if child[along[1]] != node[along[1]] or child[along[0]] != node[along[0]]:
                problems.append(f'child does not span its node: {child} in {node}')
            if child[across[0]] != position:
                problems.append(f'child out of place ({position}): {child} in {node}')
            position = child[across[0]] + child[across[1]] + 1
        if position - 1 != node[across[0]] + node[across[1]]:
            problems.append(f'children do not fill node: {node}')


class FloatServer(Server):
    def layout(self):
        return Layout(self.text('display', '-p', '#{window_layout}'))

    def panes(self):
        rows = [line.split() for line in self.text('list-panes', '-F', PANES).splitlines()]
        return {row[0]: {'floating': row[1] == '1', 'x': int(row[2]), 'y': int(row[3]),
                         'w': int(row[4]), 'h': int(row[5])} for row in rows}

    def check(self, test, label):
        self.run('has-session')
        text = self.text('display', '-p', '#{window_layout}').strip()
        layout = Layout(text)
        test.assertEqual(layout.problems, [], f'{label}: {text}')
        panes = self.panes()
        test.assertEqual(set(panes), set(layout.leaves), f'{label}: {text}')
        # A zoomed window reports its saved layout; the panes show the zoom.
        if self.text('display', '-p', '#{window_zoomed_flag}').strip() == '1':
            return layout
        # Each group's panes sit next to each other in the z-index.
        order = self.text('list-panes', '-F', '#{pane_z} #{pane_id}').split('\n')
        z = {row.split()[1]: int(row.split()[0]) for row in order if row}
        for group in layout.groups:
            members = sorted(z[p] for p, (_, g) in layout.leaves.items() if g is group)
            test.assertEqual(members, list(range(members[0], members[0] + len(members))),
                             f'{label}: z-index {z} {text}')
        for pane, (cell, group) in layout.leaves.items():
            test.assertEqual(panes[pane]['floating'], group is not None, f'{label}: {pane} {text}')
            if group is not None:
                geometry = {k: panes[pane][k] for k in 'xywh'}
                test.assertEqual(geometry, {k: cell[k] for k in 'xywh'}, f'{label}: {pane} {text}')
        # The layout string must restore the same tree. Upstream tmux leaves
        # zero-size nodes when every pane under them floats, and can tile a
        # pane into a full window past its minimum size; neither parses.
        # The Differential test holds rmux to upstream for those cases.
        if '"w":0,' in text or '"h":0,' in text or layout.main_problems:
            return layout
        self.run('select-layout', text)
        again = self.text('display', '-p', '#{window_layout}').strip()
        test.assertEqual(comparable(json.loads(again)['L']), comparable(json.loads(text)['L']),
                         f'{label}: round trip')
        return layout


class Groups(unittest.TestCase):
    def server(self):
        server = FloatServer(RMUX)
        server.__enter__()
        self.addCleanup(server.__exit__)
        return server

    def test_split_inside_a_float_makes_a_group(self):
        s = self.server()
        s.run('new-pane', '-x', '62', '-y', '22', '-X', '10', '-Y', '5')
        float_pane = s.text('display', '-p', '#{pane_id}').strip()
        s.run('split-window', '-G', '-h', '-t', float_pane)
        layout = s.check(self, 'split')
        self.assertEqual(len(layout.groups), 1)
        group = layout.groups[0]
        self.assertEqual((group['x'], group['y'], group['w'], group['h']), (11, 6, 60, 20))
        self.assertEqual(len(group['c']), 2)

        # A member split stays in the group without -G.
        member = group['c'][1]['I']
        s.run('split-window', '-v', '-t', member)
        layout = s.check(self, 'member split')
        self.assertEqual(len(layout.groups), 1)
        self.assertEqual(sum(1 for _, g in layout.leaves.values() if g is not None), 3)

    def test_stock_split_of_a_lone_float_is_unchanged(self):
        s = self.server()
        s.run('new-pane', '-x', '40', '-y', '12', '-X', '10', '-Y', '5')
        s.run('split-window', '-h')
        layout = s.check(self, 'stock split')
        self.assertEqual(layout.groups, [])
        self.assertEqual(sum(1 for _, g in layout.leaves.values() if g is not None), 2)

    def test_closing_members_collapses_the_group(self):
        s = self.server()
        s.run('new-pane', '-x', '62', '-y', '22', '-X', '10', '-Y', '5')
        first = s.text('display', '-p', '#{pane_id}').strip()
        s.run('split-window', '-G', '-h', '-t', first)
        second = s.text('display', '-p', '#{pane_id}').strip()
        s.run('split-window', '-G', '-v', '-t', second)
        s.check(self, 'three members')
        s.run('kill-pane', '-t', second)
        layout = s.check(self, 'two members')
        self.assertEqual(len(layout.groups), 1)
        s.run('kill-pane', '-t', first)
        layout = s.check(self, 'one member')
        self.assertEqual(layout.groups, [])
        survivor = [p for p, (_, g) in layout.leaves.items() if g is not None]
        self.assertEqual(len(survivor), 1)
        cell = layout.leaves[survivor[0]][0]
        self.assertEqual((cell['x'], cell['y'], cell['w'], cell['h']), (11, 6, 60, 20))

    def test_move_and_resize_apply_to_the_whole_group(self):
        s = self.server()
        s.run('new-pane', '-x', '62', '-y', '22', '-X', '10', '-Y', '5')
        first = s.text('display', '-p', '#{pane_id}').strip()
        s.run('split-window', '-G', '-h', '-t', first)
        s.run('move-pane', '-t', first, '-X', '20', '-Y', '8')
        layout = s.check(self, 'moved')
        group = layout.groups[0]
        self.assertEqual((group['x'], group['y']), (21, 9))
        s.run('move-pane', '-t', first, '-P', 'bottom-right')
        s.check(self, 'placed')
        s.run('resize-window', '-x', '70', '-y', '20')
        layout = s.check(self, 'window shrunk')
        group = layout.groups[0]
        self.assertLessEqual(group['x'] + group['w'], 70)
        self.assertLessEqual(group['y'] + group['h'], 20)

    def test_tiling_a_member_leaves_the_rest_floating(self):
        s = self.server()
        s.run('new-pane', '-x', '62', '-y', '22', '-X', '10', '-Y', '5')
        first = s.text('display', '-p', '#{pane_id}').strip()
        s.run('split-window', '-G', '-h', '-t', first)
        second = s.text('display', '-p', '#{pane_id}').strip()
        s.run('split-window', '-G', '-v', '-t', second)
        s.run('join-pane', '-s', second, '-t', second)
        layout = s.check(self, 'tiled one')
        self.assertIsNone(layout.leaves[second][1])
        self.assertEqual(len(layout.groups), 1)

    def test_zoom_and_layout_presets_keep_the_group(self):
        s = self.server()
        s.run('split-window', '-h')
        s.run('new-pane', '-x', '62', '-y', '22', '-X', '10', '-Y', '5')
        first = s.text('display', '-p', '#{pane_id}').strip()
        s.run('split-window', '-G', '-h', '-t', first)
        before = s.check(self, 'before zoom').groups[0]
        s.run('resize-pane', '-Z', '-t', first)
        s.run('resize-pane', '-Z', '-t', first)
        self.assertEqual(geometry(s.check(self, 'after zoom').groups[0]), geometry(before))
        for preset in ['even-horizontal', 'even-vertical', 'main-vertical', 'main-horizontal', 'tiled']:
            s.run('select-layout', preset)
            layout = s.check(self, preset)
            self.assertEqual(len(layout.groups), 1, preset)


def checksum(layout):
    csum = 0
    for char in layout:
        csum = (csum >> 1) + ((csum & 1) << 15)
        csum = (csum + ord(char)) & 0xffff
    return f'{csum:04x},{layout}'


class Edges(unittest.TestCase):
    """Paths the design review flagged as easy to get wrong."""

    def server(self):
        server = FloatServer(RMUX)
        server.__enter__()
        self.addCleanup(server.__exit__)
        return server

    def group(self, s, *extra):
        s.run('new-pane', '-x', '62', '-y', '22', '-X', '10', '-Y', '5')
        first = s.text('display', '-p', '#{pane_id}').strip()
        s.run('split-window', '-G', '-h', '-t', first, *extra)
        second = s.text('display', '-p', '#{pane_id}').strip()
        return first, second

    def test_new_float_from_a_member_goes_beside_the_group(self):
        s = self.server()
        self.group(s)
        s.run('new-pane')
        layout = s.check(self, 'new float')
        self.assertEqual(len(layout.groups[0]['c']), 2)
        s.run('select-layout', 'tiled')
        s.check(self, 'preset')

    def test_first_member_closing_leaves_the_group_rectangle(self):
        s = self.server()
        first, second = self.group(s)
        before = s.check(self, 'group').groups[0]
        s.run('kill-pane', '-t', first)
        cell = s.check(self, 'survivor').leaves[second][0]
        self.assertEqual([cell[k] for k in 'xywh'], [before[k] for k in 'xywh'])

    def test_group_beside_a_float_only_node(self):
        s = self.server()
        s.run('split-window', '-h')
        s.run('split-window', '-v')
        left, top, bottom = s.text('list-panes', '-F', '#{pane_id}').split()
        s.run('new-pane', '-t', bottom)
        s.run('new-pane', '-t', bottom)
        floats = [p for p, i in s.panes().items() if i['floating']]
        s.run('kill-pane', '-t', top)
        s.run('kill-pane', '-t', bottom)
        s.run('split-window', '-G', '-h', '-t', floats[1])
        s.check(self, 'group made')
        s.run('resize-pane', '-t', left, '-R', '5')
        s.check(self, 'tiled resize')
        s.run('join-pane', '-s', floats[0], '-t', floats[0])
        s.check(self, 'tiled a float')
        s.run('resize-window', '-x', '60', '-y', '20')
        s.check(self, 'window shrunk')
        s.run('resize-window', '-x', '150', '-y', '45')
        s.check(self, 'window grown')

    def test_group_split_without_room_changes_nothing(self):
        s = self.server()
        s.run('new-pane', '-x', '30', '-y', '4', '-X', '10', '-Y', '5')
        pane = s.text('display', '-p', '#{pane_id}').strip()
        before = s.text('display', '-p', '#{window_layout}')
        result = s.run('split-window', '-G', '-v', '-t', pane, check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(s.text('display', '-p', '#{window_layout}'), before)
        s.check(self, 'unchanged')

    def test_group_split_is_refused_while_zoomed(self):
        s = self.server()
        s.run('split-window', '-h')
        s.run('new-pane', '-A', '-x', '40', '-y', '12')
        over = s.text('display', '-p', '#{pane_id}').strip()
        s.run('resize-pane', '-Z', '-t', '%0')
        s.run('select-pane', '-t', over)
        s.run('split-window', '-G', '-h', '-t', over, check=False)
        s.run('resize-pane', '-Z', '-t', '%0', check=False)
        s.check(self, 'after zoom')

    def test_window_with_only_a_group(self):
        s = self.server()
        first, second = self.group(s)
        s.run('kill-pane', '-t', '%0')
        layout = s.check(self, 'only group')
        self.assertEqual(len(layout.groups), 1)
        for size in (('50', '15'), ('140', '45'), ('30', '10')):
            s.run('resize-window', '-x', size[0], '-y', size[1])
            s.check(self, f'resized {size}')
        s.run('new-pane')
        s.check(self, 'new float')

    def test_swap_and_z_moves_keep_groups_whole(self):
        s = self.server()
        s.run('split-window', '-h')
        first, second = self.group(s)
        s.run('new-pane', '-x', '30', '-y', '8')
        s.run('swap-pane', '-D', '-t', '%0')
        s.check(self, 'swap down')
        s.run('swap-pane', '-U', '-t', '%1')
        s.check(self, 'swap up')
        for position in ('forward', 'backward', 'front', 'back', 'forward-loop', 'backward-loop'):
            s.run('move-pane', '-t', second, '-P', position)
            s.check(self, position)
        s.run('move-pane', '-t', second, '-z', '1')
        s.check(self, 'z 1')
        s.run('select-pane', '-t', first)
        s.check(self, 'raised')

    def test_v1_layout_keeps_groups(self):
        s = self.server()
        s.run('split-window', '-h')
        self.group(s)
        s.run('select-layout', checksum('120x40,0,0{60x40,0,0,0,59x40,61,0,1}'))
        layout = s.check(self, 'v1')
        self.assertEqual(len(layout.groups), 1)

    def test_join_into_a_group(self):
        s = self.server()
        s.run('split-window', '-h')
        first, second = self.group(s)
        s.run('join-pane', '-v', '-s', '%1', '-t', second)
        layout = s.check(self, 'joined')
        self.assertIs(layout.leaves['%1'][1], layout.groups[0])

    def test_member_resize_moves_the_separator_or_the_group(self):
        s = self.server()
        first, second = self.group(s)
        s.run('resize-pane', '-t', first, '-R', '4')
        layout = s.check(self, 'separator')
        self.assertEqual(layout.leaves[first][0]['w'], 34)
        height = layout.groups[0]['h']
        s.run('resize-pane', '-t', first, '-D', '3')
        layout = s.check(self, 'group taller')
        self.assertEqual(layout.groups[0]['h'], height + 3)
        s.run('resize-pane', '-t', second, '-y', '10')
        s.check(self, 'group to size')


class ReviewFindings(Edges):
    """Reproductions from the final review, kept as regressions."""

    def test_display_panes_with_a_group_off_screen(self):
        s = self.server()
        s.run('resize-window', '-x', '80', '-y', '24')
        s.run('new-pane', '-x', '20', '-y', '12', '-X', '10', '-Y', '5')
        s.run('split-window', '-G', '-v')
        s.run('move-pane', '-R', '1500000000', '-D', '7', '-t', '%1')
        s.run('display-panes', '-t', '%0', check=False)
        s.run('move-pane', '-Y', '23', '-t', '%1', check=False)
        s.run('display-panes', '-t', '%0', check=False)
        s.check(self, 'server alive')

    def test_a_hidden_group_member_is_not_active_while_zoomed(self):
        s = self.server()
        s.run('split-window', '-h')
        first, second = self.group(s)
        s.run('new-pane', '-A', '-x', '30', '-y', '8', '-X', '60', '-Y', '20')
        over = s.text('display', '-p', '#{pane_id}').strip()
        s.run('join-pane', '-s', over, '-t', second, '-v')
        s.run('resize-pane', '-Z', '-t', '%0')
        self.assertEqual(s.text('display', '-p', '#{pane_id}').strip(), '%0')

    def test_tiling_without_room_keeps_the_group(self):
        s = self.server()
        s.run('resize-window', '-x', '12', '-y', '10')
        s.run('split-window', '-h')
        s.run('split-window', '-h')
        s.run('new-pane', '-x', '8', '-y', '6', '-X', '1', '-Y', '1')
        s.run('split-window', '-G', '-v')
        member = s.text('display', '-p', '#{pane_id}').strip()
        result = s.run('join-pane', '-s', member, '-t', member, check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(len(s.check(self, 'kept').groups), 1)

    def test_rotate_window_keeps_groups_together(self):
        s = self.server()
        s.run('split-window', '-h')
        self.group(s)
        s.run('new-pane', '-x', '30', '-y', '10', '-X', '20', '-Y', '8')
        for direction in ('-D', '-U', '-D'):
            s.run('rotate-window', direction)
            s.check(self, f'rotate {direction}')


class Stress(unittest.TestCase):
    """Seeded random operation sequences; the tree must stay valid throughout."""

    def pick(self, rng, panes, floating=None):
        choices = [p for p, info in panes.items() if floating is None or info['floating'] == floating]
        return rng.choice(choices) if choices else None

    def step(self, s, rng):
        panes = s.panes()
        op = rng.randrange(16)
        if op == 0 or len(panes) < 2:
            args = ['new-pane']
            if rng.random() < .5:
                args += ['-x', f'{rng.randrange(20, 90)}%', '-y', f'{rng.randrange(20, 90)}%']
            return args
        if op in (1, 2):
            target = self.pick(rng, panes, True)
            if target:
                return ['split-window', '-G', rng.choice(['-h', '-v']),
                        *(['-b'] if rng.random() < .3 else []), '-t', target]
        if op == 3:
            target = self.pick(rng, panes, False)
            if target:
                return ['split-window', rng.choice(['-h', '-v']), '-t', target]
        if op == 4 and len(panes) > 2:
            return ['kill-pane', '-t', self.pick(rng, panes)]
        if op == 5:
            target = self.pick(rng, panes, True)
            if target:
                return ['join-pane', '-s', target, '-t', target]
        if op == 6:
            target = self.pick(rng, panes, False)
            if target:
                return ['break-pane', '-W', '-s', target]
        if op == 7:
            return ['resize-pane', '-t', self.pick(rng, panes),
                    rng.choice(['-L', '-R', '-U', '-D']), str(rng.randrange(1, 8))]
        if op == 8:
            target = self.pick(rng, panes, True)
            if target:
                return ['move-pane', '-t', target, '-P',
                        rng.choice(['top-left', 'centre', 'bottom-right', 'front', 'back'])]
        if op == 9:
            return ['resize-window', '-x', str(rng.randrange(40, 160)), '-y', str(rng.randrange(12, 50))]
        if op == 10:
            return ['resize-pane', '-Z', '-t', self.pick(rng, panes)]
        if op == 11:
            return ['select-layout', rng.choice(['even-horizontal', 'even-vertical', 'main-vertical',
                                                 'main-horizontal', 'tiled'])]
        if op == 12:
            return ['swap-pane', '-s', self.pick(rng, panes), '-t', self.pick(rng, panes)]
        if op == 13:
            target = self.pick(rng, panes, True)
            if target:
                return ['resize-pane', '-t', target, '-x', str(rng.randrange(10, 80)),
                        '-y', str(rng.randrange(5, 30))]
        if op == 14:
            target = self.pick(rng, panes, True)
            source = self.pick(rng, panes)
            if target and source != target:
                return ['join-pane', rng.choice(['-h', '-v']), '-s', source, '-t', target]
        if op == 15:
            target = self.pick(rng, panes, True)
            if target:
                return ['move-pane', '-t', target, rng.choice(['-z', '-X', '-Y']), str(rng.randrange(0, 20))]
        return ['select-pane', '-t', self.pick(rng, panes)]

    def test_random_sequences_keep_a_valid_tree(self):
        for seed in range(6):
            rng = random.Random(seed)
            with FloatServer(RMUX) as s:
                history = []
                for number in range(120):
                    args = self.step(s, rng)
                    history.append(' '.join(args))
                    s.run(*args, check=False)
                    s.check(self, f'seed {seed} step {number}: ' + ' | '.join(history[-6:]))


class Differential(unittest.TestCase):
    """Without groups rmux must lay out floating and tiled panes exactly like
    upstream tmux, including upstream's own edge cases."""

    def test_stock_sequences_match_upstream(self):
        stress = Stress()
        for seed in range(8):
            rng = random.Random(1000 + seed)
            with FloatServer(RMUX) as ours, FloatServer(BASELINE) as theirs:
                history = []
                for number in range(120):
                    args = stress.step(ours, rng)
                    if '-G' in args:
                        args.remove('-G')
                    history.append(' '.join(args))
                    a = ours.run(*args, check=False)
                    b = theirs.run(*args, check=False)
                    label = f'seed {seed} step {number}: ' + ' | '.join(history[-4:])
                    self.assertEqual((a.returncode, a.stderr), (b.returncode, b.stderr), label)
                    self.assertEqual(ours.text('display', '-p', '#{window_layout}'),
                                     theirs.text('display', '-p', '#{window_layout}'), label)
                    self.assertEqual(ours.text('list-panes', '-F', '#{pane_id} #{pane_z} #{pane_active}'),
                                     theirs.text('list-panes', '-F', '#{pane_id} #{pane_z} #{pane_active}'),
                                     label)


if __name__ == '__main__':
    unittest.main()
