# kj workspace — workspace management

Workspaces group filesystem paths for mounting into contexts.

```bash
kj workspace list
kj workspace show my-project
kj workspace create my-project --desc "Main project" --path /home/user/src/project
kj workspace add my-project /home/user/src/lib --mount /mnt/lib
kj workspace bind my-project .
kj workspace remove old-ws    # latched
```
