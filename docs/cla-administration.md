# CLA administration

## Activate

1. Obtain legal review of [`CLA.md`](../CLA.md), including the Project Owner,
   patent grant, Apache-2.0 commitment, and entity process.
2. Publish the approved `CLA.md` in a public Gist with
   [`cla-assistant-metadata.json`](cla-assistant-metadata.json) as a second file
   named `metadata`.
3. Connect the Gist and `bmp4070/jabar-lsp` at
   [CLA Assistant](https://cla-assistant.io/).
4. Publish a privacy notice and private contact for signing records.
5. Test with an external account, then require the CLA check on `main`.

## Maintain

- Archive each accepted agreement revision with its signing records. A text or
  metadata change requires a new revision and renewed acceptance.
- Allowlist only service accounts that cannot sign.
- For entity acceptance, privately verify the legal entity name, signer's
  authority, and covered GitHub accounts. The hosted form cannot require these
  fields conditionally, so review them before merging entity-owned work.
- Require every human contributor to accept individually.
- Export signing records periodically and restrict access to them.
