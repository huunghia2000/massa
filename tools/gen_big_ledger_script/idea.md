# Idea

Get the deployment options from `deploy.toml` of the labnet

Connect to the labnet's API in local (clone the repo on the remote server, and start locally)

Spawn a lot of new addresses everywhere, add bytecode to make them bigger

Estimate the size of the ledger based on the number of addresses created

Once arrive to the targetted size, quit

The binary is wrapped in a script

If the binary quits without an error, restarts a distant node, bootstraping from the other one

Add debug to get metrics on the bootstraping time
