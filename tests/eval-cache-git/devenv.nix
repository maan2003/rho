{ lib, ... }:
let
  # Read a file to test caching behavior
  fileContent = builtins.readFile ./test-file.txt;
in
{
  # Create a simple environment for testing
  packages = [ ];

  # Use the file content in the environment
  env.FILE_CONTENT = fileContent;
}
