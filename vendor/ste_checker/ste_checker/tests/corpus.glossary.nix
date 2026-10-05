# Technical Names and Technical Verbs a Rust/Nix command-line project may declare under
# ASD-STE100. Only words that name software artifacts or operations on them belong here —
# ordinary prose that STE wants rewritten does not.
#
# A Technical Name is approved as a noun and a Technical Verb as a verb, so a word the project
# uses both ways is declared twice.
{
  names = [
    "adapter"
    "book"
    "build"
    "channel"
    { name = "check"; desc = "one run of a lint rule over one file"; }
    "config"
    "cpi"
    "daemon"
    "default"
    "enrollment"
    "error"
    "false"
    "fee"
    "file"
    "flag"
    "function"
    "gate"
    "gdp"
    "governance"
    "implementation"
    "index"
    "inflation"
    "input"
    "latency"
    { name = "leg"; desc = "one side of a two-venue trade"; }
    "literacy"
    "loopback"
    "maker"
    "output"
    "percentile"
    "quote"
    "release"
    "snapshot"
    "target"
    "threshold"
    "true"
    { name = "venue"; desc = "an exchange the runtime holds a connection to"; }
  ];

  verbs = [
    "boot"
    "build"
    "check"
    "copy"
    "delete"
    "enable"
    "gate"
    "generate"
    "restart"
    "start"
    "switch"
    { name = "tick"; desc = "advance the clock by one quote"; }
    "validate"
  ];
}
