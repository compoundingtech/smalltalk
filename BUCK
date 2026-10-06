# Generated file - DO NOT EDIT
# Source: BUCK.genie.ts

load("@prelude//toolchains:genrule.bzl", "system_genrule_toolchain")
toolchain_alias(name = "rust", actual = "//buck2/toolchains:rust", visibility = ["PUBLIC"])
toolchain_alias(name = "cxx", actual = "//buck2/toolchains:cxx", visibility = ["PUBLIC"])
toolchain_alias(name = "go_bootstrap", actual = "//buck2/toolchains:go_bootstrap", visibility = ["PUBLIC"])
toolchain_alias(name = "python_bootstrap", actual = "//buck2/toolchains:python_bootstrap", visibility = ["PUBLIC"])
system_genrule_toolchain(name = "genrule", visibility = ["PUBLIC"])
alias(name = "package_tree_runtime", actual = "@rules//:package_tree_runtime", visibility = ["PUBLIC"])
alias(name = "package_command_runtime", actual = "@rules//:package_command_runtime", visibility = ["PUBLIC"])

filegroup(
    name = "typecheck",
    srcs = {
        "clients/typescript/st3-client/typecheck": "//clients/typescript/st3-client:typecheck",
        "clients/typescript/st3-client/typecheck_schema": "//clients/typescript/st3-client:typecheck_schema",
        "clients/typescript/st3-views/typecheck": "//clients/typescript/st3-views:typecheck",
    },
    visibility = ["PUBLIC"],
)
