//! Project-relative paths derived from a selected Python file.

use super::*;
use std::path::Component;

const PATH_TYPES: [&str; 6] = [
    "pathlib.Path",
    "pathlib.PurePath",
    "pathlib.PurePosixPath",
    "pathlib.PureWindowsPath",
    "pathlib.PosixPath",
    "pathlib.WindowsPath",
];

pub(super) fn is_pathlib_member(module: &str, member: &str) -> bool {
    module == "pathlib"
        && matches!(
            member,
            "Path" | "PurePath" | "PurePosixPath" | "PureWindowsPath" | "PosixPath" | "WindowsPath"
        )
        || PATH_TYPES.contains(&module)
            && matches!(
                member,
                "__new__" | "__init__" | "__getattribute__" | "parent" | "resolve" | "__truediv__"
            )
}

impl Graph<'_> {
    pub(super) fn source_project_path(
        &self,
        scope: ScopeId,
        before: Location,
        arguments: &[Expression],
        keywords: &[(String, Expression)],
    ) -> Option<PathBuf> {
        let [argument] = arguments else {
            return None;
        };
        let file = self.scopes[scope].file;
        let source = &self.files[file];
        if scope != file
            || !keywords.is_empty()
            || !matches!(&argument.kind, ExpressionKind::Name(name) if name == "__file__")
            || self.local(scope, "__file__", before).is_some()
            || self.scopes[scope].open_wildcard
            || self
                .module_member_write(&source.module, "__file__", (scope, before))
                .is_some()
            || !self.pathlib_members_stable((scope, before), &["__new__", "__init__"])
        {
            return None;
        }
        let path = Path::new(&source.path);
        project_relative(path).then(|| path.to_owned())
    }

    pub(super) fn project_path_member(
        &self,
        path: PathBuf,
        name: &str,
        access: (ScopeId, Location),
    ) -> Value {
        if !self.pathlib_members_stable(access, &[name, "__getattribute__"]) {
            return Value::Unknown;
        }
        match name {
            "parent" => path
                .parent()
                .map(|parent| Value::ProjectPath(parent.to_owned()))
                .unwrap_or(Value::Unknown),
            "resolve" => Value::ResolveProjectPath(path),
            _ => Value::Unknown,
        }
    }

    pub(super) fn join_project_path(
        &self,
        path: PathBuf,
        right: &Expression,
        access: (ScopeId, Location),
    ) -> Value {
        let ExpressionKind::Text(relative) = &right.kind else {
            return Value::Unknown;
        };
        let relative = Path::new(relative);
        if project_relative(relative) && self.pathlib_members_stable(access, &["__truediv__"]) {
            Value::ProjectPath(path.join(relative))
        } else {
            Value::Unknown
        }
    }

    fn pathlib_members_stable(&self, access: (ScopeId, Location), members: &[&str]) -> bool {
        self.imported_module_write("pathlib", access).is_none()
            && [
                "PurePosixPath",
                "PureWindowsPath",
                "PosixPath",
                "WindowsPath",
            ]
            .iter()
            .all(|name| self.module_member_write("pathlib", name, access).is_none())
            && PATH_TYPES.iter().all(|provider| {
                members
                    .iter()
                    .all(|name| self.module_member_write(provider, name, access).is_none())
            })
    }
}

fn project_relative(path: &Path) -> bool {
    path.components()
        .all(|component| matches!(component, Component::Normal(_)))
}
