#!/usr/bin/env python3
"""Exercise documentation's filename, URL, HTML and browser-DOM boundaries."""
import json
from pathlib import Path
import subprocess
import sys
import tempfile
from html.parser import HTMLParser
from urllib.parse import quote, unquote, urlsplit


class Page(HTMLParser):
    def __init__(self, text):
        super().__init__(convert_charrefs=True)
        self.links = []
        self.ids = []
        self.feed(text)

    def handle_starttag(self, tag, attrs):
        attrs = dict(attrs)
        assert all(not k.startswith('on') or (tag == 'form' and k == 'onsubmit'
                   and v == 'return false;') for k, v in attrs.items()), attrs
        assert tag not in ('img', 'iframe', 'svg'), tag
        if tag == 'a' and 'href' in attrs:
            self.links.append(attrs['href'])
        if 'id' in attrs:
            self.ids.append(attrs['id'])


def page_file(name, ns, ext):
    component = quote(name, safe='-._~').replace('.', '%2E').replace('~', '%7E')
    if name.lower() == 'index':
        component = '~' + component
    return component + ('.' + ns if ns else '') + '.' + ext


def main():
    plum = str(Path(sys.argv[1] if len(sys.argv) > 1 else './sh').resolve())
    with tempfile.TemporaryDirectory(prefix='plum-doc-output-') as tmp:
        work = Path(tmp)
        project = work / 'project'
        # '/' cannot occur in a directory entry. The payload tests the same
        # attribute/markup boundary with a quote and a real event-handler name.
        names = ['ordinary', 'x\"><img src=x onerror=alert(1)>', 'é雪#?%&', '100%20real', 'javascript:alert(1)', 'index', '~index', 'foo', 'foo.Thing', 'foo%2EThing']
        for name in names:
            folder = project / name
            folder.mkdir(parents=True)
            (folder / 'main.plum').write_text('pub let find_me (): Int = 1\npub let Thing.method (): Int = 2\n')
        (project / 'ordinary' / 'records.plum').write_text(
            'pub struct Managed { value: Int }\n'
            'pub extern "C" struct Header { value: U8 }\n'
            'pub extern "C" struct Reading { value: U16 }\n'
            'pub enum State { Ready, Waiting }\n'
            'pub handle Token { on_drop: release_token }\n'
            'extern "C" { fn release_token(token: Int); }\n')

        def doc(source, out, *flags, success=True):
            result = subprocess.run([plum, 'doc', str(source), '-o', str(out), *flags],
                                    capture_output=True, text=True, timeout=120)
            assert (result.returncode == 0) == success, result.stdout + result.stderr
            if not success:
                assert 'plum doc:' in result.stdout, result.stdout
                assert 'wrote ' not in result.stdout, result.stdout
                assert not out.exists() or not list(out.iterdir()), out
            return result.stdout

        html = work / 'html'
        home = '../home?x="quoted"&y=\'single\'#雪'
        doc(project, html, '--html', '--home', 'Docs <img>=' + home)
        ordinary = Page((html / 'ordinary.html').read_text())
        for anchor in ['find_me', 'Managed', 'Header', 'Reading']:
            assert ordinary.ids.count(anchor) == 1, ordinary.ids
        search = (html / 'search-index.js').read_text().removeprefix('window.PLUM_DOCS = ').removesuffix(';\n')
        records = {row['n']: row['u'] for row in json.loads(search) if row['s'].startswith('extern "C" struct ')}
        assert records == {'Header': 'ordinary.html#Header', 'Reading': 'ordinary.html#Reading'}, records
        for name in names:
            for ns in ['', 'Thing']:
                filename = page_file(name, ns, 'html')
                page = Page((html / filename).read_text())
                assert home in page.links
                for link in page.links:
                    if link == home:
                        continue
                    url = urlsplit(link)
                    assert not url.scheme and not url.netloc, link
                    target = html / unquote(url.path)
                    assert target.is_file(), (link, target)
                    if url.fragment:
                        assert unquote(url.fragment) in Page(target.read_text()).ids
        landing = Page((html / 'index.html').read_text())
        for name in names:
            filename = page_file(name, '', 'html')
            assert quote(filename, safe='-._~') in landing.links
        subprocess.run(['node', str(Path(__file__).with_name('doc-search-test.js')),
                        str(html)], check=True, timeout=30)

        for i, url in enumerate(['javascript:alert(1)', 'JaVaScRiPt:alert(1)', 'data:text/html,hi',
                                 'java\nscript:alert(1)', '\tjavascript:alert(1)', '//host/path',
                                 '\\host/path', 'https://host/\rbad', 'https://host/ bad', '']):
            doc(project, work / f'unsafe-{i}', '--html', '--home', 'Bad=' + url, success=False)
        for i, url in enumerate(['https://example.test/?a=1&b=2', 'HTTP://example.test/',
                                 '/index.html', './a:b', '#fragment', '?query=one:two']):
            target = work / f'safe-{i}'
            doc(project, target, '--html', '--home', 'Home=' + url)
            assert url in Page((target / 'index.html').read_text()).links

        markdown = work / 'md'
        doc(project, markdown)
        index = (markdown / 'index.md').read_text()
        declarations = [urlsplit(line.rsplit('](', 1)[1][:-1]) for line in index.splitlines()
                        if line.startswith('- [`')]
        assert {link.fragment for link in declarations if link.path == 'ordinary.md'} == {
            'find_me', 'Managed', 'Header', 'Reading', 'State', 'Token'}
        for link in declarations:
            target = markdown / unquote(link.path)
            anchors = [line for line in target.read_text().splitlines() if line.startswith('<a id=')]
            assert Page('\n'.join(anchors)).ids.count(unquote(link.fragment)) == 1, link
        # The hostile project also contains index, ~index, foo.Thing and
        # namespace Thing in foo: all identities must survive independently.
        for ext, folder in [('html', html), ('md', markdown)]:
            for name in names:
                for ns in ['', 'Thing']:
                    assert (folder / page_file(name, ns, ext)).is_file()
            assert len(list(folder.glob('*.' + ext))) == 1 + 2 * len(names)
        # Reject remaining aliases on case-insensitive filesystems before
        # writing either output format.
        for case, mods in [('case', ['Foo', 'foo'])]:
            source = work / case
            for name in mods:
                (source / name).mkdir(parents=True)
                (source / name / 'main.plum').write_text(
                    'pub let find_me (): Int = 1\npub let Thing.method (): Int = 2\n')
            for ext, flags in [('md', []), ('html', ['--html'])]:
                log = doc(source, work / f'collision-{case}-{ext}', *flags, success=False)
                assert 'filename collision' in log, log
    print('doc-output-test: HTML, URLs, home policy, search DOM and collision checks pass')


if __name__ == '__main__':
    main()
