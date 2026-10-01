#!/usr/bin/env python3
"""Generate the server's typed GitHub write handlers; no network access needed.

github-openapi.json is a compact subset of the pinned official OpenAPI files.
--refresh MAIN GHEC LICENSE rebuilds it from that commit's upstream files.
Request objects reject undeclared fields unless additionalProperties explicitly
permits them. Reads use the client metadata as a GET/path allowlist, and responses
are relayed without generated models.
"""
import argparse
import copy
import hashlib
import json
from pathlib import Path
import re
import subprocess

ROOT = Path(__file__).resolve().parent
SPEC = ROOT.parent / "rho-notebook/src/ghapi/gh_spec.json"
SOURCE = ROOT / "github-openapi.json"
OUTPUT = ROOT / "src/api/gh_generated.rs"
PIN = "2b78fb0c53617f188e45979ecefd83b521c5428a"
SKIP = {"description", "summary", "example", "examples", "externalDocs",
        "deprecated", "tags", "operationId"}
KEY_MAPS = {"properties", "schemas", "parameters", "responses", "headers", "content"}


def compact(value, named=False):
    if isinstance(value, list):
        return [compact(v) for v in value]
    if isinstance(value, dict):
        return {k: compact(v, k in KEY_MAPS) for k, v in value.items()
                if named or (k not in SKIP and not k.startswith("x-"))}
    return value


def operations(source):
    return {op["operationId"]: (path, method, op)
            for path, methods in source["paths"].items()
            for method, op in methods.items()
            if isinstance(op, dict) and "operationId" in op}


def opid(op):
    return op["group"].replace("_", "-") + "/" + op["name"].replace("_", "-")


def metadata():
    return [o for o in json.loads(SPEC.read_text())["ops"]
            if o["verb"] != "GET" and (o["group"], o["name"]) != ("pulls", "set_draft")]


def refresh(main_file, enterprise_file, license_file):
    main, enterprise = [json.loads(Path(p).read_text())
                        for p in (main_file, enterprise_file)]
    sources = operations(main), operations(enterprise)
    result = {
        "openapi": main["openapi"],
        "info": {"title": "GitHub REST API typed-handler source",
                 "version": main["info"]["version"]},
        "x-source": {
            "repository": "https://github.com/github/rest-api-description",
            "commit": PIN,
            "files": ["descriptions/api.github.com/api.github.com.2022-11-28.json",
                      "descriptions/ghec/ghec.json"],
            "license": Path(license_file).read_text().strip()},
        "paths": {}, "components": {}}

    def copy_refs(value, source, ghec):
        if isinstance(value, list):
            return [copy_refs(v, source, ghec) for v in value]
        if not isinstance(value, dict):
            return value
        target = {}
        for k, v in value.items():
            if k == "$ref":
                assert v.startswith("#/components/"), v
                _, _, kind, name = v.split("/")
                dest = "ghec-" + name if ghec else name
                components = result["components"].setdefault(kind, {})
                if dest not in components:
                    components[dest] = {}
                    components[dest] = copy_refs(
                        compact(source["components"][kind][name]), source, ghec)
                target[k] = f"#/components/{kind}/{dest}"
            else:
                target[k] = copy_refs(v, source, ghec)
        return target

    for meta in sorted(metadata(), key=opid):
        key = opid(meta)
        ghec = key not in sources[0]
        source = enterprise if ghec else main
        path, method, op = sources[int(ghec)][key]
        item = copy_refs(compact({k: v for k, v in op.items() if k != "responses"}), source, ghec)
        item["operationId"] = key
        parameters = source["paths"][path].get("parameters", [])
        if parameters:
            item["parameters"] = copy_refs(compact(parameters), source, ghec) + item.get("parameters", [])
        result["paths"].setdefault(path, {})[method] = item
    SOURCE.write_text(json.dumps(result, sort_keys=True, separators=(",", ":")) + "\n")


def camel(name):
    parts = re.findall(r"[A-Za-z0-9]+", name)
    value = "".join(p[:1].upper() + p[1:] for p in parts)
    return ("N" if value[:1].isdigit() else "") + value


def ident(name):
    result = re.sub(r"[^a-zA-Z0-9_]", lambda m: "_x" + format(ord(m.group()), "x") + "_", name)
    if not result or result[0].isdigit():
        result = "field_" + result
    # Prefix every field: no Rust keywords, special self/type fields, or collisions.
    return "field_" + result


def lit(value):
    return json.dumps(value, ensure_ascii=True)


class Generator:
    def __init__(self, source):
        self.source = source
        self.definitions = {}
        self.cache = {}
        self.refs = {}
        self.handlers = []
        self.aliases = []

    def resolve(self, schema):
        while "$ref" in schema:
            ref = schema["$ref"]
            assert ref.startswith("#/components/")
            _, _, kind, name = ref.split("/")
            schema = self.source["components"][kind][name]
        return schema

    def merge(self, left, right):
        """Intersect object schemas, including required lists and nested properties."""
        left, right = self.normalize(left), self.normalize(right)
        if not left:
            return copy.deepcopy(right)
        if not right:
            return copy.deepcopy(left)
        result = copy.deepcopy(left)
        for key, value in right.items():
            if key == "properties":
                props = result.setdefault(key, {})
                for name, prop in value.items():
                    props[name] = self.merge(props[name], prop) if name in props else copy.deepcopy(prop)
            elif key == "required":
                result[key] = sorted(set(result.get(key, [])) | set(value))
            elif key == "nullable":
                result[key] = value and left.get("nullable", True)
            elif key == "enum" and key in result:
                result[key] = [v for v in result[key] if v in value]
            elif key == "additionalProperties":
                if value is False or result.get(key) is False:
                    result[key] = False
                elif isinstance(value, dict) and isinstance(result.get(key), dict):
                    result[key] = self.merge(result[key], value)
                else:
                    result[key] = value
            elif key in ("oneOf", "anyOf") and key in result and result[key] != value:
                raise ValueError(f"unsupported intersecting {key}")
            elif key == "type" and key in result and result[key] != value:
                raise ValueError(f"incompatible types: {result[key]} / {value}")
            else:
                result[key] = copy.deepcopy(value)
        # Null belongs to an intersection only if every typed member allows it.
        if (left.get("type") or left.get("properties")) and (right.get("type") or right.get("properties")):
            result["nullable"] = bool(left.get("nullable") and right.get("nullable"))
        return result

    def normalize(self, schema):
        schema = copy.deepcopy(self.resolve(schema))
        if "allOf" in schema:
            branches = schema.pop("allOf")
            nullable = schema.pop("nullable", False)
            base = {}
            for branch in branches:
                base = self.merge(base, branch)
            schema = self.merge(base, schema)
            if nullable:
                schema["nullable"] = True
        # enum-only discriminator branches still have a concrete primitive type.
        if "enum" in schema and "type" not in schema:
            vals = [x for x in schema["enum"] if x is not None]
            if vals and all(isinstance(x, str) for x in vals):
                schema["type"] = "string"
        return schema

    def named(self, schema, name):
        typ = self.typ(schema, name)
        self.aliases.append(f"pub(super) type {name} = {typ};")
        return name

    def typ(self, schema, hint="Model"):
        if "$ref" in schema:
            ref = schema["$ref"]
            key = ref
            if key not in self.refs:
                name = camel(ref.rsplit("/", 1)[-1]) + "Input"
                # References are always boxed: recursive component graphs stay finite.
                self.refs[key] = name
                self.definitions[name] = ""
                actual = self.typ(self.resolve(schema), name)
                self.definitions[name] = f"pub(super) type {name} = {actual};"
            typ = f"Box<{self.refs[key]}>"
            return f"Option<{typ}>" if schema.get("nullable") else typ
        schema = self.normalize(schema)
        nullable = schema.pop("nullable", False)
        typ = self.nonnull(schema, hint)
        return f"Option<{typ}>" if nullable else typ

    def nonnull(self, schema, hint):
        combinator = next((k for k in ("oneOf", "anyOf") if k in schema), None)
        if combinator:
            base = {k: v for k, v in schema.items() if k != combinator}
            # Required-only branches constrain the common object, rather than
            # representing unconstrained JSON alternatives.
            branches = [self.merge(base, s) for s in schema[combinator]]
            branches.sort(key=lambda s: (-len(s.get("required", [])),
                                         -len(s.get("properties", {}))))
            if len(branches) == 1:
                return self.typ(branches[0], hint)
            key = (json.dumps({"union": branches}, sort_keys=True))
            if key in self.cache:
                return self.cache[key]
            name = self.unique(hint + "Union", key)
            self.cache[key] = name
            variants = [self.typ(s, name + f"Variant{i}") for i, s in enumerate(branches)]
            variants = list(dict.fromkeys(variants))
            self.definitions[name] = (
                "#[derive(Deserialize, Serialize)]\n#[serde(untagged)]\n"
                f"pub(super) enum {name} {{\n" +
                "\n".join(f"    V{i}({typ})," for i, typ in enumerate(variants)) + "\n}")
            return name
        kind = schema.get("type")
        if kind == "string":
            values = schema.get("enum")
            if values and all(isinstance(v, str) for v in values):
                key = ("enum", tuple(values))
                if key in self.cache:
                    return self.cache[key]
                name = self.unique(hint + "Enum", key)
                self.cache[key] = name
                self.definitions[name] = (
                    "#[derive(Deserialize, Serialize)]\n"
                    f"pub(super) enum {name} {{\n" +
                    "\n".join(f"    #[serde(rename = {lit(v)})]\n    V{i}," for i, v in enumerate(values)) + "\n}")
                return name
            return "String"
        if kind in ("integer", "number", "boolean", "null"):
            return {"integer": "i64", "number": "f64", "boolean": "bool", "null": "()"}[kind]
        if kind == "array":
            return f"Vec<{self.typ(schema['items'], hint + 'Item')}>"
        if kind == "object" or "properties" in schema or "required" in schema or "additionalProperties" in schema:
            props = dict(schema.get("properties", {}))
            # A required but undeclared property is genuinely unconstrained in
            # OpenAPI. Do not guess its shape or silently ignore requiredness.
            for name in schema.get("required", []):
                props.setdefault(name, {})
            additional = schema.get("additionalProperties")
            if not props and additional is not False and schema.get("maxProperties") != 0:
                item = self.typ(additional, hint + "Value") if isinstance(additional, dict) else "serde_json::Value"
                return f"std::collections::BTreeMap<String, {item}>"
            key = (json.dumps(schema, sort_keys=True))
            if key in self.cache:
                return self.cache[key]
            name = self.unique(hint + "Object", key)
            self.cache[key] = name
            self.definitions[name] = ""
            fields = []
            required = set(schema.get("required", []))
            seen = set()
            for field, sub in sorted(props.items()):
                if sub.get("readOnly"):
                    continue
                rust_field = ident(field)
                assert rust_field not in seen, (name, field)
                seen.add(rust_field)
                typ = self.typ(sub, name + camel(field))
                attrs = [f"rename = {lit(field)}"]
                if field not in required:
                    typ = f"Optional<{typ}>"
                    attrs += ["default", 'skip_serializing_if = "Optional::is_missing"']
                if field in required:
                    attrs.append('deserialize_with = "required"')
                fields.append(f"    #[serde({', '.join(attrs)})]\n    pub(super) {rust_field}: {typ},")
            if additional is True or isinstance(additional, dict):
                typ = self.typ(additional, name + "Additional") if isinstance(additional, dict) else "serde_json::Value"
                fields.append(f"    #[serde(flatten)]\n    pub(super) additional: std::collections::BTreeMap<String, {typ}>,")
            strict = (additional is not True and not isinstance(additional, dict)) or schema.get("maxProperties") == 0
            self.definitions[name] = (
                "#[derive(Deserialize, Serialize)]\n" +
                ("#[serde(deny_unknown_fields)]\n" if strict else "") +
                f"pub(super) struct {name} {{\n" + "\n".join(fields) + "\n}")
            return name
        if kind is not None:
            raise ValueError(f"unsupported type {kind!r}")
        # No declared primitive/object/array/union shape means arbitrary JSON.
        return "serde_json::Value"

    def unique(self, hint, key):
        digest = hashlib.sha256(repr(key).encode()).hexdigest()[:10]
        return hint[:90] + digest.title()

    def parameter_schema(self, op, location):
        properties, required = {}, []
        for parameter in op.get("parameters", []):
            parameter = self.resolve(parameter)
            if parameter["in"] != location:
                continue
            properties[parameter["name"]] = parameter.get("schema", {})
            if parameter.get("required"):
                required.append(parameter["name"])
        return {"type": "object", "properties": properties, "required": required} if properties else None

    def generate_operation(self, meta, op):
        fn = meta["group"] + "_" + meta["name"]
        prefix = camel(fn)
        schema = self.parameter_schema(op, "path")
        params = [self.named(schema, prefix + "Path") if schema else "()"]
        body = self.resolve(op["requestBody"]) if "requestBody" in op else {}
        content = body.get("content", {})
        json_body = next((v for k, v in content.items() if k == "application/json" or k.endswith("+json")), None)
        params.append(self.named(json_body.get("schema", {}), prefix + "Body") if json_body is not None else "()")
        self.handlers.append(
            f"pub(super) fn {fn}(request: super::Request) -> super::HandlerFuture {{\n"
            "    Box::pin(async move {\n"
            f"        super::typed::<{', '.join(params)}>(request, {str(bool(body.get('required'))).lower()}, "
            f"{str(json_body is not None).lower()}).await\n"
            "    })\n}\n")

    def generate(self):
        api = operations(self.source)
        metas = sorted(metadata(), key=opid)
        assert set(api) == {opid(m) for m in metas}, "metadata/OpenAPI coverage drift"
        for meta in metas:
            path, method, op = api[opid(meta)]
            assert path == meta["path"] and method.upper() == meta["verb"], opid(meta)
            try:
                self.generate_operation(meta, op)
            except Exception as error:
                raise ValueError(f"{opid(meta)}: {error}") from error
        header = f"""// @generated by generate-gh.py; do not edit.
// GitHub REST OpenAPI commit {PIN}; MIT license in github-openapi.json.
// {len(metas)} typed write handlers; reads and responses are not generated.
#![allow(dead_code, non_camel_case_types, non_snake_case)]
use serde::{{Deserialize, Serialize}};
use super::Optional;

// deserialize_with suppresses serde's implicit missing = None for required
// nullable fields, including nullable references hidden behind type aliases.
fn required<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where D: serde::Deserializer<'de>, T: Deserialize<'de> {{
    T::deserialize(deserializer)
}}
"""
        match = ["pub(super) fn handler(group: &str, name: &str) -> Option<super::Handler> {",
                 "    match (group, name) {"]
        for meta in metas:
            fn = meta["group"] + "_" + meta["name"]
            match.append(f"        ({lit(meta['group'])}, {lit(meta['name'])}) => Some({fn}),")
        match.extend(["        _ => None,", "    }", "}"])
        return "\n\n".join([header, *[v for _, v in sorted(self.definitions.items())],
                            *self.aliases, *self.handlers, "\n".join(match)]) + "\n"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--refresh", nargs=3, metavar=("MAIN", "GHEC", "LICENSE"))
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    if args.refresh:
        refresh(*args.refresh)
    source = json.loads(SOURCE.read_text())
    assert source["x-source"]["commit"] == PIN
    generated = Generator(source).generate()
    # Format via stdin, so --check never mutates the checked-in file.
    formatted = subprocess.run(
        ["rustfmt", "--edition", "2024", "--emit", "stdout"], input=generated,
        text=True, capture_output=True, check=True).stdout
    if args.check:
        if OUTPUT.read_text() != formatted:
            raise SystemExit("gh_generated.rs is stale; run generate-gh.py")
        print("generated handlers are deterministic and current")
    else:
        OUTPUT.write_text(formatted)
        print(f"generated {len(operations(source))} typed handlers in {OUTPUT}")


if __name__ == "__main__":
    main()
