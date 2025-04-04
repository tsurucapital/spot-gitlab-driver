A simple [GitLab custom
executor](https://docs.gitlab.com/runner/executors/custom/). All it does is pick
up jobs from `gitlab-runner`, spawn a spot instance, run scripts `gitlab-runner`
provides and shuts down an instance. No ASG to manage, to warm instances.

This isn't meant to be used externally as-is: it contains hardcoded values and
basically whatever we need. It does illustrate that a basic use-case is simple.
The official autoscaling solution provided GitLab is [full of
bugs](https://gitlab.com/gitlab-org/gitlab/-/issues/408131): I spent longer
adding debugging to the autoscaler/taskscaler than just writing an alternative
that works for us.