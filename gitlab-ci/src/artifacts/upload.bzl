"""
Rules to manipulate with artifacts: download, upload etc.
"""

load("@bazel_skylib//rules:common_settings.bzl", "BuildSettingInfo")

def _upload_artifact_impl(ctx):
    """
    Uploads an artifact to s3 and returns download link to it

    ctx.version_file contains the information written by workspace_status_command.
    Bazel treats this file as never changing  - the rule only rebuilds when other dependencies change.
    Details are on https://bazel.build/docs/user-manual#workspace-status.
    """

    uploader = ctx.actions.declare_file(ctx.label.name + "_uploader")

    rclone_config = ctx.file.rclone_config
    rclone_endpoint = ctx.attr._s3_endpoint[BuildSettingInfo].value
    if rclone_endpoint != "":
        rclone_config = ctx.file.rclone_anon_config

    ctx.actions.expand_template(
        template = ctx.file._artifacts_uploader_template,
        output = uploader,
        substitutions = {
            "@@RCLONE@@": ctx.file._rclone.path,
            "@@RCLONE_CONFIG@@": rclone_config.path,
            "@@REMOTE_SUBDIR@@": ctx.attr.remote_subdir,
            "@@VERSION_FILE@@": ctx.version_file.path,
        },
        is_executable = True,
    )

    out = []

    for f in ctx.files.inputs:
        filesum = ctx.actions.declare_file(ctx.label.name + "/" + f.basename + ".SHA256SUM")
        ctx.actions.run_shell(
            command = "(cd {path} && shasum --algorithm 256 --binary {src}) > {out}".format(path = f.dirname, src = f.basename, out = filesum.path),
            inputs = [f],
            outputs = [filesum],
        )
        out.append(filesum)

    checksum = ctx.actions.declare_file(ctx.label.name + "/SHA256SUMS")
    ctx.actions.run_shell(
        command = "cat " + " ".join([f.path for f in out]) + " | sort -k 2 >" + checksum.path,
        inputs = out,
        outputs = [checksum],
    )

    fileurl = []
    for f in ctx.files.inputs + [checksum]:
        filename = ctx.label.name + "_" + f.basename
        url = ctx.actions.declare_file(filename + ".url")
        proxy_cache_url = ctx.actions.declare_file(filename + ".proxy-cache-url")
        ctx.actions.run(
            executable = uploader,
            arguments = [f.path, url.path, proxy_cache_url.path],
            env = {
                "RCLONE_S3_ENDPOINT": rclone_endpoint,
                "VERSION": ctx.attr._ic_version[BuildSettingInfo].value,
            },
            inputs = [f, ctx.version_file, rclone_config],
            outputs = [url, proxy_cache_url],
            tools = [ctx.file._rclone],
        )
        fileurl.extend([url, proxy_cache_url])

    urls = ctx.actions.declare_file(ctx.label.name + ".urls")
    ctx.actions.run_shell(
        command = "cat " + " ".join([url.path for url in fileurl]) + " >" + urls.path,
        inputs = fileurl,
        outputs = [urls],
    )
    out.append(urls)
    out.extend(fileurl)

    executable = ctx.actions.declare_file(ctx.label.name + ".bin")
    ctx.actions.write(output = executable, content = "#!/bin/sh\necho;exec cat " + urls.short_path, is_executable = True)

    return [DefaultInfo(files = depset(out), runfiles = ctx.runfiles(files = out), executable = executable)]

_upload_artifacts = rule(
    implementation = _upload_artifact_impl,
    executable = True,
    attrs = {
        "inputs": attr.label_list(allow_files = True),
        "remote_subdir": attr.string(mandatory = True),
        "rclone_config": attr.label(allow_single_file = True, default = "//:.rclone.conf"),
        "rclone_anon_config": attr.label(allow_single_file = True, default = "//:.rclone-anon.conf"),
        "_rclone": attr.label(allow_single_file = True, default = "@rclone//:rclone"),
        "_artifacts_uploader_template": attr.label(allow_single_file = True, default = ":upload.bash.template"),
        "_ic_version": attr.label(default = "//bazel:ic_version"),
        "_s3_endpoint": attr.label(default = ":s3_endpoint"),
    },
)

def upload_artifacts(**kwargs):
    """
    Uploads artifacts to the S3 storage.

    Wrapper around _upload_artifacts to always set required tags.

    Args:
      **kwargs: all arguments to pass to _upload_artifacts
    """

    tags = kwargs.get("tags", [])
    for tag in ["requires-network", "manual"]:
        if tag not in tags:
            tags.append(tag)
    kwargs["tags"] = tags
    _upload_artifacts(**kwargs)

def urls_test(name, inputs, tags = ["manual"]):
    # https://github.com/bazelbuild/bazel/issues/6783S
    native.sh_library(
        name = name + "_wrapped",
        data = inputs,
        tags = tags,
    )
    native.sh_test(
        name = name,
        tags = tags + ["requires-network"],
        srcs = ["//gitlab-ci/src/artifacts:urls_test.sh"],
        args = ["$(rootpaths :{})".format(name + "_wrapped")],
        data = [":" + name + "_wrapped"],
    )
