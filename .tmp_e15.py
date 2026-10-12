p='ssh-provider/src/provision.rs'
s=open(p,encoding='utf-8',newline='').read()
crlf='\r\n' in s
s=s.replace('\r\n','\n')
i=s.index("    #[test]\n    fn upstream_branch_name_strips")
j=s.index("    #[test]\n    fn redact_url_from_error_text_scrubs",i)
s=s[:i]+s[j:]
i=s.index("    #[test]\n    fn build_provision_script_uses_origin_as_the_remote_name_regardless_of_upstream_form")
j=s.index("    #[test]\n    fn build_provision_script_checks_lock_before_any_git_command",i)
s=s[:i]+'''    #[test]
    fn build_provision_script_tracks_the_upstream_branch_on_origin() {
        let script = build_provision_script(args(
            "/r/p",
            "/r/p/repository",
            "/r/p/worktrees",
            "/r/p/worktrees/w",
            "/r/p/.provision.lock",
            "/r/p/metadata.json",
            "proj",
            "https://example.invalid/x.git",
            "feat",
            "release/1.2",
        ));
        // This provider's fresh clone's remote is always called "origin".
        assert!(script.contains("'origin/release/1.2'"), "{script}");
    }

'''+s[j:]
if crlf: s=s.replace('\n','\r\n')
open(p,'w',encoding='utf-8',newline='').write(s)
