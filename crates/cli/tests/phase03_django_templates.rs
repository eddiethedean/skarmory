use std::{fs, path::Path, process::Command};

use serde_json::Value;

#[test]
fn selected_django_template_follows_lexical_context_and_fresh_source() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    let cache = tempfile::tempdir().unwrap();
    for (path, source) in [
        (".skylos.toml", "root = true\n"),
        (
            "manage.py",
            "import os\nfrom django.core.management import execute_from_command_line\nos.environ.setdefault('DJANGO_SETTINGS_MODULE', 'settings')\nexecute_from_command_line([])\n",
        ),
        (
            "settings.py",
            "ROOT_URLCONF = 'urls'\nINSTALLED_APPS = ['app']\nTEMPLATES = [{'BACKEND': 'django.template.backends.django.DjangoTemplates', 'APP_DIRS': True}]\n",
        ),
        (
            "urls.py",
            "from django.urls import path\nfrom app.views import Report\nurlpatterns = [path('', Report.as_view())]\n",
        ),
        ("app/__init__.py", ""),
        (
            "app/models.py",
            "from django.db.models import Model\nclass Record(Model):\n    def total(self): return helper()\n    def retired(self): return 1\nclass Other(Model):\n    def total(self): return 2\ndef helper(): return 0\n",
        ),
        (
            "app/views.py",
            "from django.views.generic import DetailView\nfrom app.models import Record\nclass Report(DetailView):\n    model = Record\n",
        ),
        (
            "app/templates/app/record_detail.html",
            "{% with object=other %}{% include 'app/partial.html' %}{% endwith %}{% for object in others %}{{ object.retired }}{% empty %}{{ object.total }}{% endfor %}",
        ),
        ("app/templates/app/partial.html", "{{ object.retired }}"),
    ] {
        let target = root.join(path);
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(target, source).unwrap();
    }

    let report = scan(root, None);
    assert_eq!(report["coverage"]["status"], "complete");
    assert!(has_unused(&report, "app.models.Other.total"));
    assert!(has_unused(&report, "app.models.Record.retired"));
    assert!(!has_unused(&report, "app.models.Record.total"));
    assert!(!has_unused(&report, "app.models.helper"));
    let symbols = report["dead_code_evidence"]["symbols"].as_array().unwrap();
    assert!(symbols.iter().any(|symbol| {
        symbol["qualified_name"] == "app.models.Record.total"
            && symbol["evidence"].as_array().unwrap().iter().any(|edge| {
                edge["kind"] == "django_template_method" && edge["certainty"] == "possible"
            })
    }));
    assert_eq!(scan(root, Some(cache.path())), report);

    fs::write(
        root.join("app/templates/app/record_detail.html"),
        "literal text",
    )
    .unwrap();
    let revised = scan(root, Some(cache.path()));
    assert_eq!(revised, scan(root, None));
    assert!(has_unused(&revised, "app.models.Record.total"));
    assert!(has_unused(&revised, "app.models.helper"));

    fs::write(
        root.join("app/templates/app/record_detail.html"),
        "{% unmodeled object %}",
    )
    .unwrap();
    let opaque = scan(root, None);
    assert_eq!(opaque["coverage"]["status"], "incomplete");
    assert!(
        opaque["coverage"]["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["kind"] == "unresolved_django_template")
    );

    fs::create_dir_all(root.join("project/pages/app")).unwrap();
    fs::create_dir_all(root.join("pages/app")).unwrap();
    fs::write(root.join("project/__init__.py"), "").unwrap();
    fs::write(
        root.join("project/loader.py"),
        "from pathlib import Path as Location\n\
         BASE = Location(__file__).resolve().parent\n\
         TEMPLATES = [{'BACKEND': 'django.template.backends.django.DjangoTemplates', \
                        'DIRS': [BASE / 'pages']}]\n",
    )
    .unwrap();
    fs::write(
        root.join("settings.py"),
        "from project.loader import TEMPLATES\nROOT_URLCONF = 'urls'\nINSTALLED_APPS = ['app']\n",
    )
    .unwrap();
    fs::write(
        root.join("project/pages/app/record_detail.html"),
        "{{ object.total }}",
    )
    .unwrap();
    fs::write(
        root.join("pages/app/record_detail.html"),
        "{{ object.retired }}",
    )
    .unwrap();
    let imported = scan(root, Some(cache.path()));
    assert_eq!(imported, scan(root, None));
    assert_eq!(imported["coverage"]["status"], "complete");
    assert!(!has_unused(&imported, "app.models.Record.total"));
    assert!(has_unused(&imported, "app.models.Record.retired"));

    fs::write(root.join("urls.py"), "urlpatterns = []\n").unwrap();
    let inactive = scan(root, None);
    assert_eq!(inactive["coverage"]["status"], "complete");
    assert!(has_unused(&inactive, "app.models.Record.total"));
}

fn scan(root: &Path, cache: Option<&Path>) -> Value {
    let mut command = Command::new(env!("CARGO_BIN_EXE_skarmory"));
    command
        .env_remove("DJANGO_SETTINGS_MODULE")
        .env_remove("SKYLOS_CONFIG_FILE")
        .current_dir(root)
        .args([
            "--profile",
            "skylos-0.3",
            ".",
            "--json",
            "--no-grep-verify",
            "--confidence",
            "0",
        ]);
    if let Some(cache) = cache {
        command.args(["--workers", "4", "--cache-dir"]).arg(cache);
    } else {
        command.args(["--workers", "1"]);
    }
    let output = command.output().unwrap();
    assert!(matches!(output.status.code(), Some(0 | 2)), "{output:?}");
    let mut report: Value = serde_json::from_slice(&output.stdout).unwrap();
    report.as_object_mut().unwrap().remove("cache");
    report
}

fn has_unused(report: &Value, name: &str) -> bool {
    report["unused_functions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|row| row["full_name"] == name)
        || report["suppressed"].as_array().unwrap().iter().any(|row| {
            row["full_name"] == name && row["reason"] == "covered by the unused class finding"
        })
}
