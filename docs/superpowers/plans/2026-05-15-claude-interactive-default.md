# Claude Interactive Default Plan

Superseded.

Claude interactive mode is the default Claude launch path. Fresh interactive
sessions must let Claude choose the session id, discover the newly-created
session JSONL under the profile's `projects/<encoded-cwd>/` directory, and tail
that discovered file.

The old documented opt-out path has been removed from CLI help and launch
dispatch. Keep future fixes focused on making interactive mode reliable.
