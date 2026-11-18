"""Read compiler query output, rather than infer syntax from source layout."""
import json
import re
import subprocess


class Literal(str):
    """A quoted string in the compiler's normalized S-expression AST."""


def parse_ast(text):
    # Parse iteratively: real compiler ASTs can contain deeply nested blocks.
    # These are tokens of the AST query format, not tokens of Plum source.
    roots = []
    stack = [roots]
    end = 0
    for match in re.finditer(r'\s+|[()]|"(?:\\.|[^"\\])*"|[^()\s"]+', text):
        if match.start() != end:
            raise ValueError("invalid AST token")
        end = match.end()
        token = match[0]
        if token.isspace():
            continue
        if token == "(":
            node = []
            stack[-1].append(node)
            stack.append(node)
        elif token == ")":
            if len(stack) == 1:
                raise ValueError("unexpected closing AST delimiter")
            stack.pop()
        elif token.startswith('"'):
            stack[-1].append(Literal(json.loads(token, strict=False)))
        else:
            stack[-1].append(token)
    if end != len(text) or len(stack) != 1 or len(roots) != 1 or not isinstance(roots[0], list) or not roots[0]:
        raise ValueError("expected one nonempty program AST")
    return roots[0]


def query(compiler, *args):
    result = subprocess.run([str(compiler), *map(str, args)], capture_output=True, text=True, timeout=90)
    if result.returncode or not result.stdout.strip():
        raise ValueError(f"plum {' '.join(map(str, args))} failed: {result.stderr.strip() or result.stdout.strip()}")
    return result.stdout


def ast(compiler, path):
    return parse_ast(query(compiler, "ast", path))


def nodes(tree):
    pending = [tree]
    while pending:
        node = pending.pop()
        if isinstance(node, list):
            yield node
            pending.extend(reversed(node))


def function_body(program, name):
    matches = []
    for item in program:
        if isinstance(item, list) and item and item[0] == "let":
            i = 2 if len(item) > 1 and item[1] == "pub" else 1
            if len(item) > i and item[i] == name:
                matches.append(item[-1])
    if len(matches) != 1:
        raise ValueError(f"expected one definition of {name}")
    return matches[0]


def comparison_names(tree, variable):
    names = set()
    for node in nodes(tree):
        if len(node) == 3 and node[0] == "==":
            for left, right in [(node[1], node[2]), (node[2], node[1])]:
                if type(left) is str and left == variable and isinstance(right, Literal):
                    names.add(str(right))
    if not names:
        raise ValueError(f"no {variable} comparisons in compiler AST")
    return names


def inventory_names(program, function, record):
    body = function_body(program, function)
    if not isinstance(body, list) or not body or body[0] != "array":
        raise ValueError(f"{function} is not a literal inventory")
    names = set()
    for item in body[1:]:
        if not isinstance(item, list) or item[:2] != ["struct-lit", record]:
            raise ValueError(f"unexpected {function} entry")
        fields = [item[i + 1] for i in range(2, len(item) - 1) if type(item[i]) is str and item[i] == "name="]
        if len(fields) != 1 or not isinstance(fields[0], Literal) or not fields[0]:
            raise ValueError(f"missing literal name in {function}")
        names.add(str(fields[0]))
    if not names:
        raise ValueError(f"empty {function} inventory")
    return names


def completion_names(text):
    data = json.loads(text)
    if not isinstance(data, list) or not data or any(
        not isinstance(item, dict) or not isinstance(item.get("name"), str) or not item["name"] for item in data
    ):
        raise ValueError("expected nonempty completion records with names")
    return {item["name"] for item in data}
