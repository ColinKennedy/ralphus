import re, sys
REMOTES = {'origin', 'alt', 'upstream', 'fork', 'gitlab', 'github', 'mirror'}
pat = re.compile(r'\?upstream=((?:<<[a-z_]+>>)|[^&"\s>{}\\)]*)')

def conv(m):
    v = m.group(1)
    if v == '<<current_branch>>':
        return '?upstream_remote=.&upstream_branch=<<current_branch>>'
    if v.startswith('<<'):
        return '?upstream_remote=origin&upstream_branch=' + v
    if '/' in v:
        head, tail = v.split('/', 1)
        if head in REMOTES and tail:
            return f'?upstream_remote={head}&upstream_branch={tail}'
    return f'?upstream_remote=.&upstream_branch={v}'

for p in sys.argv[1:]:
    s = open(p, encoding='utf-8').read()
    n = pat.sub(conv, s)
    if n != s:
        open(p, 'w', encoding='utf-8').write(n)
        print('rewrote', p)
