# The task role policy for ECS Anywhere

[detector-policy.json](./detector-policy.json) grants a task role the three permissions the detector needs on ECS Anywhere, and nothing else.
Attach it to the task role of every task that should report its managed instance.
Substitute `<region>`, `<account-id>`, and `<cluster>` throughout; a cluster per policy keeps each one narrow.

The detector runs without the policy. Every permission it lacks costs the attributes behind it and leaves a notice on standard error.

## What each statement does

`ReadTheTaskAndTheContainerInstanceHoldingIt` allows the two ECS calls against one cluster's tasks and container instances.
Neither call names a resource of its own in older, short-form ARNs, so the `ecs:cluster` condition confines them where the resource ARNs cannot.

`ReadTheTagsOnAManagedInstance` allows the Systems Manager call against `managed-instance/mi-*`.
Only a node registered from outside AWS carries an `mi-` ID, so the pattern reaches every ECS Anywhere host and no EC2 instance, whose tags live under an `ec2` ARN instead.

## Why the two `Deny` statements

An `Allow` grants what it names; it does not withhold what a second policy grants.
The two `Deny` statements withhold the rest, so a role that also carries a broad policy still reads one cluster and one kind of Systems Manager resource:

- `DenyBothCallsAgainstAnyOtherCluster` turns on `ArnNotEquals`, which a request missing `ecs:cluster` satisfies. A call that cannot say which cluster it means is therefore denied along with a call that names the wrong one.
- `DenyThatCallAgainstAnyOtherResource` turns on `NotResource`, which reaches every Systems Manager resource but the managed instances.

Both `Deny` statements bind the whole principal, not this policy alone.
A role that lists tags on parameters or documents for some other purpose loses that, so give the detector a role of its own.
