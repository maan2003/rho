# Technical Names and Technical Verbs this project owns. ASD-STE100 section 1, rule 1.5.
# A name is approved as a noun, a verb as a verb; a word used both ways appears twice.
#
# Most of these are the vocabulary of grammar, which this program is about and the standard's
# approved wordlist has no reason to carry.
{
  names = [
    "aerospace"
    "approved"
    "cluster"
    "compound"
    "config"
    "configuration"
    "contraction"
    "default"
    "description"
    "error"
    "file"
    "flag"
    { name = "glossary"; desc = "docs/glossary.nix, the Technical Names and Technical Verbs a project declares"; }
    "input"
    "license"
    { name = "markdown"; desc = "the lightweight markup language the checker reads"; }
    "microsecond"
    "model"
    { name = "nix"; desc = "the package manager and its language"; }
    "noun"
    "output"
    "parser"
    "passive"
    "process"
    "program"
    "rule"
    "sentence"
    "specification"
    "speech"
    "splitter"
    { name = "ste"; desc = "ASD-STE100, Simplified Technical English"; }
    "tag"
    "technical"
    "text"
    "verb"
    "vocabulary"
    "word"
    "wordlist"
  ];

  verbs = [
    "set"
    { name = "simplify"; desc = "write again in Simplified Technical English"; }
    "switch"
    { name = "tag"; desc = "write a part of speech onto a word"; }
  ];
}
