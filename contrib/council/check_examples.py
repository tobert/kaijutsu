#!/usr/bin/env python3
"""Check docs/council-api.openapi.yaml against example and malformed bodies.

Run from the repo root:
  uv run --no-project --with jsonschema --with pyyaml contrib/council/check_examples.py

Every case states whether the schema must accept or reject it; exit 1 on any surprise.
"""
import sys
import referencing.jsonschema
import yaml
from jsonschema import Draft202012Validator
from referencing import Registry, Resource
doc = yaml.safe_load(open('docs/council-api.openapi.yaml'))
root = {"$id": "urn:council", "components": doc["components"]}
reg = Registry().with_resource("urn:council", Resource.from_contents(root, default_specification=referencing.jsonschema.DRAFT202012))
def check(name, inst, ok=True, why=""):
    v = Draft202012Validator({"$ref": f"urn:council#/components/schemas/{name}"}, registry=reg)
    errs = list(v.iter_errors(inst)); good = (not errs) == ok
    print("PASS" if good else "FAIL", name, why, "" if good else [e.message[:120] for e in errs][:3]); return good
U1="0199b3c4-6c1e-7a2b-9f00-3e5d1c2a7b11"; U2="0199b3c4-6c1e-7a2b-9f00-3e5d1c2a7b12"
S="sha256:"+"9c"*32; SN="snap:"+"4b"*32; H="e3"*32
spec={"name":"shell-gate","instructions":"Judge.","input_label":"Proposed statement","questions":[
 {"id":"effect","type":"text","instructions":"What it does.","max_tokens":48},
 {"id":"undo","type":"score","instructions":"How hard.","criteria":["easy","hard","impossible"]},
 {"id":"verdict","type":"choice","instructions":"What happens.","criteria":[{"option":"allow","means":"a"},{"option":"ask","means":"b"},{"option":"report","means":"c"}]}]}
ident={"model":"qwen3.8-flash-next","weight_hash":"w","tokenizer_hash":"t","template":"mk-menu-v1","engine":"e","spec_id":S}
read_v={"type":"choice","choice":"ask","probabilities":{"allow":0.05,"ask":0.90,"report":0.05},"logprobs":{"allow":-3.0,"ask":-0.105,"report":-3.0},"mass":-0.0001,"confidence":0.90}
resp={"model":"qwen3.8-flash-next",
 "answers":{"verdict":{"type":"choice","choice":"ask","probabilities":{"allow":0.04,"ask":0.74,"report":0.22},"confidence":0.74,"agree":False,"spread":0.55,
   "leave_one_out":{U1:{"allow":0.02,"ask":0.38,"report":0.60},U2:{"allow":0.05,"ask":0.90,"report":0.05}}}},
 "reads":[{"context":U1,"snapshot":SN,"described":{"effect":"Publishes."},"answers":{"verdict":read_v},"rendered_sha256":H},
          {"context":U2,"snapshot":SN,"answers":{"verdict":read_v},"rendered_sha256":H}],
 "pool":{"method":"loglinear","weights":"mass","normalized":{"verdict":[0.5,0.5]}},
 "signals":{"control_text":[]},"identity":ident,"usage":{"input_tokens":41,"output_tokens":0,"fed_tokens":212}}
plain_resp={"model":"m","answers":{"intent":{"type":"choice","choice":"bug","probabilities":{"bug":0.9,"other":0.1},"confidence":0.88,"agree":True,"spread":0}},
 "reads":[{"context":None,"snapshot":None,"answers":{"intent":{"type":"choice","choice":"bug","probabilities":{"bug":0.9,"other":0.1},"confidence":0.88,"logprobs":{"bug":-0.11,"other":-2.3},"mass":-0.02}},"rendered_sha256":H}],
 "pool":{"method":"linear","weights":"uniform","normalized":{"intent":[1.0]}},"identity":ident}
bad_pooled=dict(resp); bad_pooled["answers"]={"verdict":dict(read_v)}  # per-read answer where pooled belongs
noul_conf={"type":"noul","noul":0.4,"logprobs":{"yes":-1,"no":-0.5},"mass":-0.1}
r=[check("Spec",spec,why="api example spec"),
 check("ContextPut",{"system":"s","turns":[{"role":"user","content":"x","snap":True}],"warm":[S]}),
 check("DecisionRequest",{"spec_id":S,"contexts":[{"id":U1},{"id":U2}],"state":"git push origin main","pool":{"method":"loglinear","weights":"mass"}},why="api example request"),
 check("DecisionRequest",{"model":"typesafe/jev-1.13","state":"hi","questions":{"intent":{"type":"choice","instructions":"q","criteria":{"bug_report":"a","other":"b"}}}},why="plain Decisions request"),
 check("DecisionResponse",resp,why="api example response"),
 check("DecisionResponse",plain_resp,why="plain response, no contexts"),
 check("ReadAnswer",noul_conf,why="noul read"),
 check("DecisionRequest",{"state":"x"},ok=False,why="neither spec nor questions"),
 check("DecisionRequest",{"state":"x","spec_id":S,"questions":{"a":{"type":"noul","instructions":"q"}}},ok=False,why="both"),
 check("DecisionRequest",{"state":"x","questions":{"a":{"type":"text","instructions":"q"}}},ok=False,why="inline text"),
 check("DecisionRequest",{"state":"x","questions":{"a":{"type":"noul","instructions":"q"}},"ask":["a"]},ok=False,why="ask with inline"),
 check("DecisionRequest",{"state":"x","spec_id":S,"contxts":[{"id":U1}]},ok=False,why="misspelled contexts"),
 check("DecisionRequest",{"spec_id":S,"state":"x","pool":{"weights":[0.5,0.5]}},ok=False,why="old polymorphic weights"),
 check("Spec",{**spec,"questions":[{"id":"v","type":"choice","instructions":"q","criteria":{"a":"x","b":"y"}}]},ok=False,why="spec choice as object"),
 check("Spec",{**spec,"questions":[{"id":"e","type":"text","instructions":"q"}]},ok=False,why="text without max_tokens"),
 check("Spec",{**spec,"questions":[{"id":"v","type":"score","instructions":"q","criteria":["only"]}]},ok=False,why="one level"),
 check("DecisionRequest",{"spec_id":S,"state":"x","contexts":[{"id":U1}]*9},ok=False,why="9 contexts"),
 check("DecisionResponse",bad_pooled,ok=False,why="read answer in pooled slot"),
 check("ReadAnswer",{**noul_conf,"type":"choice"},ok=False,why="choice missing fields"),
 check("DecisionRequest",{"state":"x","questions":{"a":{"type":"noul","instructions":"q","temperature":0}}},ok=False,why="inline question extra field"),
 check("ContextPutResult",{"id":U1,"head":SN,"tokens":96,"kept":0,"fed":96,"dry_run":False,"snapshots":[]},why="api PUT reply"),
 check("ContextPutResult",{"id":U1,"head":SN,"tokens":96,"kept":0,"fed":96,"snapshots":[]},ok=False,why="PUT reply without dry_run"),
 check("Turn",{"role":"assistant","content":"READY","reasoning":"I would ask before any push.","snap":True},why="assistant turn with reasoning"),
 check("Turn",{"role":"user","content":"x","reasoning":"y"},ok=False,why="reasoning on a user turn"),
]
sys.exit(0 if all(r) else 1)
