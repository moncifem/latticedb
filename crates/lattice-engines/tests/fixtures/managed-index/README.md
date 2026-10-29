# Independent LCIX v1 examples

The independently hand-framed fixtures now live in the global [format corpus](../../../../../formats/corpus/).
The [accepted layout](../../../../../formats/catalog-index-v1.md) lists their literal semantic values.
Tests consume those exact corpus bytes rather than keeping a second copy. Golden metadata identifies
these as accepted examples after the owner's separate RFC 0005 / ADR 0019 decision. Corpus metadata
alone does not provide the required guarded WAL admission or permit mixed-version access.
