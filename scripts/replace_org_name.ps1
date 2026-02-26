param (
    [Parameter(Mandatory=$true)]
    [string]$NewOrgName,

    [string]$OldOrgName = "greenticai",

    [string]$Token = $env:GH_TOKEN,

    [switch]$DryRun,

    [switch]$CreatePR
)

# 1. Проверка токена
if ($CreatePR -and -not $Token) {
    Write-Error "GitHub Token is required for creating a PR. Please provide it via -Token or set GH_TOKEN environment variable."
    exit 1
}

if ($Token) {
    $env:GH_TOKEN = $Token
}

# 2. Поиск и замена
$excludeDirs = @(".git", "target", "node_modules", ".gemini", "brain")
$files = Get-ChildItem -Recurse -File | Where-Object { 
    $path = $_.FullName
    foreach ($exclude in $excludeDirs) {
        if ($path -like "*\$exclude\*") { return $false }
    }
    return $true
}

Write-Host "Searching for '$OldOrgName' and replacing with '$NewOrgName'..." -ForegroundColor Cyan

foreach ($file in $files) {
    $content = Get-Content $file.FullName -Raw
    if ($content -match $OldOrgName) {
        if ($DryRun) {
            Write-Host "[DRY RUN] Would update $($file.FullName)" -ForegroundColor Yellow
        } else {
            Write-Host "Updating $($file.FullName)..." -ForegroundColor Green
            $newContent = $content -replace $OldOrgName, $NewOrgName
            Set-Content $file.FullName $newContent
        }
    }
}

# 3. Git и PR
if ($CreatePR -and -not $DryRun) {
    # Сначала убедимся, что мы на актуальном master
    Write-Host "Checking out master and pulling latest changes..." -ForegroundColor Cyan
    git checkout master
    git pull origin master

    $branchName = "chore/rename-org-to-$NewOrgName"
    
    Write-Host "Creating branch $branchName..." -ForegroundColor Cyan
    git checkout -b $branchName

    Write-Host "Adding changes and committing..." -ForegroundColor Cyan
    git add .
    git commit -m "chore: rename organization from $OldOrgName to $NewOrgName"

    Write-Host "Pushing branch..." -ForegroundColor Cyan
    git push origin $branchName

    Write-Host "Creating Pull Request..." -ForegroundColor Cyan
    gh pr create --title "chore: rename organization to $NewOrgName" --body "This PR renames all occurrences of $OldOrgName to $NewOrgName across the repository."
}

