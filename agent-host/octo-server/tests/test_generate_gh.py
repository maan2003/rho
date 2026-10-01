"""Regression checks for the typed GitHub handler generator."""
import importlib.util
import json
from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[1]
loader = importlib.util.spec_from_file_location("generate_gh", ROOT / "generate-gh.py")
module = importlib.util.module_from_spec(loader)
loader.loader.exec_module(module)

class GeneratorTest(unittest.TestCase):
    def generator(self):
        return module.Generator({"components": {"schemas": {}}})

    def test_nullable_union_and_free_form_fields_stay_typed(self):
        gen = self.generator()
        gen.typ({"type":"object", "properties":{
            "title":{"oneOf":[{"type":"string"},{"type":"integer"}]},
            "milestone":{"type":"integer","nullable":True},
            "comments":{"type":"array","items":{"type":"object","properties":{"line":{"type":"integer"}}, "required":["line"]}},
            "inputs":{"type":"object","additionalProperties":{"type":"string"}}},
            "required":["title"]}, "Issue")
        code="\n".join(gen.definitions.values())
        self.assertIn("#[serde(deny_unknown_fields)]",code)
        self.assertIn("Optional<Option<i64>>",code)
        self.assertIn("Vec<",code)
        self.assertIn("field_line: i64",code)
        self.assertIn("BTreeMap<String, String>",code)
        self.assertIn("#[serde(untagged)]",code)
        self.assertNotIn("serde_json::Value",code)

    def test_allof_intersection_retains_requiredness_and_nullable_refs(self):
        gen=self.generator()
        gen.source["components"]["schemas"]["Person"]={"type":"object","properties":{"login":{"type":"string"}},"required":["login"]}
        combined=gen.normalize({"allOf":[{"type":"object","properties":{"id":{"type":"integer"}},"required":["id"]},
            {"type":"object","properties":{"name":{"type":"string"}},"required":["name"]}],"nullable":True})
        self.assertEqual(combined["required"],["id","name"])
        self.assertTrue(combined["nullable"])
        self.assertTrue(gen.typ({"allOf":[{"$ref":"#/components/schemas/Person"}],"nullable":True},"Actor").startswith("Option<"))

    def test_schema_coverage_is_exactly_the_selected_rest_surface(self):
        source=json.loads((ROOT/"github-openapi.json").read_text())
        self.assertEqual(set(module.operations(source)),{module.opid(op) for op in module.metadata()})
        self.assertEqual(len(module.operations(source)),46)
        self.assertTrue(all(op["verb"] != "GET" and not op.get("query_params") for op in module.metadata()))
        self.assertTrue(all("responses" not in op for _, _, op in module.operations(source).values()))
        self.assertTrue(all(op["group"] in {"issues","pulls","search","actions","checks","repos"} for op in module.metadata()))
        self.assertNotIn("pulls/merge",module.operations(source))
        self.assertNotIn("pulls/dismiss-review",module.operations(source))
        self.assertEqual(source["x-source"]["files"][0],"descriptions/api.github.com/api.github.com.2022-11-28.json")

    def test_generated_code_contains_only_write_requests(self):
        code = module.Generator(json.loads((ROOT/"github-openapi.json").read_text())).generate()
        self.assertNotIn("ResponseCodec", code)
        self.assertNotIn("Response200", code)
        self.assertNotIn("fn pulls_get(", code)
        self.assertIn("fn issues_update_comment(", code)
        self.assertIn("fn pulls_request_reviewers(", code)

if __name__=="__main__": unittest.main()
